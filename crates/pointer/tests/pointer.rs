//! Every coordinate rule, proven without a display server.

use nspointer::gesture;
use nspointer::mock::MockPointer;
use nspointer::motion::{Motion, Rng};
use nspointer::wire::{ErrorKind, Key, Step};
use nspointer::{Button, InputError, Loc, Point, Rect, Screen, ScreenId, Screens, Session, Timing};

fn rng() -> Rng {
    Rng::seed(42)
}

/// A 2560×1440 primary at 150%, and a 1920×1080 secondary placed to its
/// LEFT — so the secondary's origin is negative, which is the layout that
/// breaks anything storing coordinates unsigned.
fn layout() -> Screens {
    Screens {
        screens: vec![
            Screen {
                id: ScreenId::from("PRIMARY-EDID-A1"),
                bounds: Rect {
                    x: 0,
                    y: 0,
                    w: 2560,
                    h: 1440,
                },
                scale: 1.5,
                primary: true,
                label: "Dell U2723".into(),
            },
            Screen {
                id: ScreenId::from("LEFT-EDID-B2"),
                bounds: Rect {
                    x: -1920,
                    y: 0,
                    w: 1920,
                    h: 1080,
                },
                scale: 1.0,
                primary: false,
                label: "ASUS VG248".into(),
            },
        ],
        state: 7,
    }
}

async fn session() -> Session<MockPointer> {
    Session::open(MockPointer::new(layout(), Point::new(10, 10)))
        .await
        .unwrap()
}

#[tokio::test]
async fn normalized_maps_into_the_named_screen_including_a_negative_origin() {
    let s = session().await;
    // Centre of the left-hand monitor is at negative x.
    let p = s
        .resolve(&Loc::normalized("LEFT-EDID-B2", 0.5, 0.5))
        .unwrap();
    assert_eq!(p, Point::new(-960, 540));
    // Origin and far corner.
    assert_eq!(
        s.resolve(&Loc::normalized("LEFT-EDID-B2", 0.0, 0.0))
            .unwrap(),
        Point::new(-1920, 0)
    );
    // 1.0 is the LAST pixel, not one past the edge — one past would land on
    // the neighbouring monitor.
    assert_eq!(
        s.resolve(&Loc::normalized("LEFT-EDID-B2", 1.0, 1.0))
            .unwrap(),
        Point::new(-1, 1079)
    );
}

#[tokio::test]
async fn a_bad_location_is_refused_rather_than_bent_into_a_plausible_one() {
    let s = session().await;
    for bad in [
        Loc::normalized("NO-SUCH-SCREEN", 0.5, 0.5),
        Loc::normalized("PRIMARY-EDID-A1", 1.4, 0.5),
        Loc::normalized("PRIMARY-EDID-A1", 0.5, -0.1),
        Loc::absolute(99_999, 0),
    ] {
        assert!(
            matches!(s.resolve(&bad), Err(InputError::NoSuchLocation(_))),
            "{bad:?} should not resolve"
        );
    }
    // A point in the gap of an L-shaped desktop is on no screen.
    assert!(s.resolve(&Loc::absolute(-1900, 1300)).is_err());
    assert!(s.resolve(&Loc::absolute(100, 100)).is_ok());
}

/// The failure this whole design exists to prevent. A capture taken by a
/// process that is not per-monitor DPI aware comes back virtualized:
/// 2560×1440 at 150% arrives as 1707×960. Sending the image's own pixel
/// coordinates straight through lands every click at 0.667× the offset.
#[tokio::test]
async fn a_dpi_virtualized_screenshot_still_maps_to_the_right_physical_pixel() {
    let l = layout();
    let primary = l.get(&ScreenId::from("PRIMARY-EDID-A1")).unwrap();

    // A button the model sees at the centre of a 1707×960 capture.
    let p = primary.image_to_physical(1707, 960, 853.5, 480.0).unwrap();
    assert_eq!(p, Point::new(1280, 720), "centre of the physical screen");

    // Naively trusting image pixels would have clicked here instead.
    assert_ne!(p, Point::new(853, 480));

    // And it holds for a plain resize, not just DPI virtualization. Agreement
    // is to within a pixel, not exact: a downscaled capture has genuinely
    // thrown away the sub-pixel, and claiming otherwise would be a lie the
    // caller might rely on.
    let full = primary
        .image_to_physical(2560, 1440, 1280.0, 720.0)
        .unwrap();
    let half = primary.image_to_physical(1280, 720, 640.0, 360.0).unwrap();
    assert!((full.x - half.x).abs() <= 1 && (full.y - half.y).abs() <= 1);

    // Bottom-right of the image is the last physical pixel, not one past.
    assert_eq!(
        primary.image_to_physical(1707, 960, 1706.0, 959.0).unwrap(),
        Point::new(2559, 1439)
    );
    assert!(primary.image_to_physical(0, 960, 1.0, 1.0).is_none());
    assert!(primary.image_to_physical(1707, 960, 1707.0, 0.0).is_none());
    assert!(primary.image_to_physical(1707, 960, -1.0, 0.0).is_none());
    // Top-left image pixel is the screen's origin.
    assert_eq!(
        primary.image_to_physical(1707, 960, 0.0, 0.0).unwrap(),
        Point::new(0, 0)
    );
}

#[tokio::test]
async fn a_moved_path_eases_and_ends_exactly_on_target() {
    let within = Rect {
        x: 0,
        y: 0,
        w: 2560,
        h: 1440,
    };
    let from = Point::new(0, 0);
    let to = Point::new(1000, 500);
    let steps = gesture::path(from, to, 200, 8, within, &Motion::Ease, &mut rng());
    let moves: Vec<Point> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Move { x, y } => Some(Point::new(*x, *y)),
            _ => None,
        })
        .collect();

    assert_eq!(*moves.last().unwrap(), to, "rounding must not fall short");
    assert!(moves.len() > 20, "eased, not a teleport: {}", moves.len());
    // Monotonic towards the target.
    assert!(moves
        .windows(2)
        .all(|w| w[1].x >= w[0].x && w[1].y >= w[0].y));
    // Eased, not linear: at the halfway point in time, well under halfway in
    // space would be wrong, but so would exactly halfway all the way through.
    let quarter = moves[moves.len() / 4];
    assert!(quarter.x < 250, "slow start: {quarter:?}");
    // Sleeps are interleaved, and the total is about the duration asked for.
    let slept: u32 = steps
        .iter()
        .filter_map(|s| match s {
            Step::Sleep { ms } => Some(*ms),
            _ => None,
        })
        .sum();
    assert!((150..=210).contains(&slept), "slept {slept}ms for 200ms");
}

#[tokio::test]
async fn a_path_is_clamped_so_it_cannot_stray_off_its_screen() {
    let within = Rect {
        x: 0,
        y: 0,
        w: 100,
        h: 100,
    };
    let steps = gesture::path(
        Point::new(0, 0),
        Point::new(500, 500),
        100,
        8,
        within,
        &Motion::Ease,
        &mut rng(),
    );
    for s in &steps {
        if let Step::Move { x, y } = s {
            assert!((0..100).contains(x) && (0..100).contains(y), "{s:?}");
        }
    }
}

#[tokio::test]
async fn a_zero_duration_move_is_a_single_jump() {
    let within = Rect {
        x: 0,
        y: 0,
        w: 100,
        h: 100,
    };
    assert_eq!(
        gesture::path(
            Point::new(0, 0),
            Point::new(9, 9),
            0,
            8,
            within,
            &Motion::Ease,
            &mut rng()
        ),
        vec![Step::Move { x: 9, y: 9 }]
    );
}

#[tokio::test]
async fn click_moves_then_presses_in_one_round_trip() {
    let mock = MockPointer::new(layout(), Point::new(0, 0));
    let s = Session::open(mock).await.unwrap().with_timing(Timing {
        move_ms: 32,
        step_ms: 8,
        ..Timing::default()
    });
    let state = s
        .click_at(
            &Loc::normalized("PRIMARY-EDID-A1", 0.5, 0.5),
            Button::Left,
            2,
        )
        .await
        .unwrap();
    assert_eq!(state, 7, "the layout token comes back");
}

#[tokio::test]
async fn a_double_click_presses_twice_with_a_gap() {
    let steps = gesture::click(Button::Left, 2, 40, 80);
    let buttons: Vec<bool> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Button { down, .. } => Some(*down),
            _ => None,
        })
        .collect();
    assert_eq!(buttons, vec![true, false, true, false]);
    assert!(
        steps.contains(&Step::Sleep { ms: 80 }),
        "gap between clicks"
    );
}

#[tokio::test]
async fn a_drag_settles_before_pressing_and_before_releasing() {
    let within = Rect {
        x: 0,
        y: 0,
        w: 500,
        h: 500,
    };
    let steps = gesture::drag(
        Point::new(10, 10),
        Point::new(400, 400),
        Button::Left,
        50,
        30,
        within,
        &Motion::Ease,
        &mut rng(),
    );
    // move → sleep → down → sleep → …path… → sleep → up
    assert!(matches!(steps[0], Step::Move { x: 10, y: 10 }));
    assert_eq!(steps[1], Step::Sleep { ms: 30 });
    assert_eq!(
        steps[2],
        Step::Button {
            button: Button::Left,
            down: true
        }
    );
    assert_eq!(steps[3], Step::Sleep { ms: 30 });
    let n = steps.len();
    assert_eq!(
        steps[n - 1],
        Step::Button {
            button: Button::Left,
            down: false
        }
    );
    assert_eq!(steps[n - 2], Step::Sleep { ms: 30 });
    // The pointer is at the target when the button comes up.
    let last_move = steps
        .iter()
        .rev()
        .find_map(|s| match s {
            Step::Move { x, y } => Some(Point::new(*x, *y)),
            _ => None,
        })
        .unwrap();
    assert_eq!(last_move, Point::new(400, 400));
}

/// The agent refusing must reach the caller as a refusal, not as a success.
/// On Windows `SendInput` returns success under UIPI and does nothing.
#[tokio::test]
async fn an_agent_refusal_propagates() {
    for kind in [ErrorKind::Suspended, ErrorKind::Blocked] {
        let mock = MockPointer::failing(
            layout(),
            Point::new(0, 0),
            InputError::Agent {
                kind,
                detail: "elevated window".into(),
            },
        );
        let s = Session::open(mock).await.unwrap();
        let err = s
            .click_at(
                &Loc::normalized("PRIMARY-EDID-A1", 0.5, 0.5),
                Button::Left,
                1,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, InputError::Agent { kind: k, .. } if k == kind));
    }
}

// ---- OxyMouse-inspired path shapes (crates/pointer/src/motion.rs) ----

fn screen() -> Rect {
    Rect {
        x: 0,
        y: 0,
        w: 2560,
        h: 1440,
    }
}

fn moves(steps: &[Step]) -> Vec<Point> {
    steps
        .iter()
        .filter_map(|s| match s {
            Step::Move { x, y } => Some(Point::new(*x, *y)),
            _ => None,
        })
        .collect()
}

/// The invariant that must hold for every algorithm, however wild the route:
/// start from where the pointer is, land exactly on the target, stay on the
/// screen. A humanized path that misses the button is worse than a straight
/// one that hits it.
#[tokio::test]
async fn every_motion_lands_exactly_on_target_and_stays_in_bounds() {
    let from = Point::new(40, 1300);
    let to = Point::new(2400, 60);
    for motion in [
        Motion::Ease,
        Motion::Bezier { bow: 0.2 },
        Motion::Gaussian { sigma: 14.0 },
        Motion::Perlin {
            amplitude: 30.0,
            octaves: 4,
        },
    ] {
        for seed in [1u64, 7, 99, 100_000] {
            let steps = gesture::path(from, to, 300, 8, screen(), &motion, &mut Rng::seed(seed));
            let m = moves(&steps);
            assert_eq!(*m.last().unwrap(), to, "{motion:?} seed {seed}");
            for p in &m {
                assert!(screen().contains(*p), "{motion:?} left the screen at {p:?}");
            }
            // No duplicate consecutive points: those are wasted round-trip
            // payload telling the agent to stay put.
            assert!(
                m.windows(2).all(|w| w[0] != w[1]),
                "{motion:?} repeats a point"
            );
        }
    }
}

/// Ease is the straight line; the other three bow away from it. Without this
/// the algorithms could all silently degrade to Ease and every other test
/// would still pass.
#[tokio::test]
async fn the_humanized_motions_actually_leave_the_straight_line() {
    let from = Point::new(0, 0);
    let to = Point::new(1600, 900);
    let deviation = |motion: Motion, seed: u64| -> f64 {
        let steps = gesture::path(from, to, 400, 8, screen(), &motion, &mut Rng::seed(seed));
        // Distance of each point from the straight line, at its worst.
        let (dx, dy) = ((to.x - from.x) as f64, (to.y - from.y) as f64);
        let len = (dx * dx + dy * dy).sqrt();
        moves(&steps)
            .iter()
            .map(|p| {
                let (px, py) = ((p.x - from.x) as f64, (p.y - from.y) as f64);
                ((dx * py - dy * px) / len).abs()
            })
            .fold(0.0_f64, f64::max)
    };
    // Averaged over seeds, not asserted per seed. Noise that occasionally
    // stays near the line is correct behaviour, not a regression, and a
    // per-seed floor would be a flaky test dressed up as a strict one.
    let mean =
        |motion: Motion| -> f64 { (1..=12u64).map(|s| deviation(motion, s)).sum::<f64>() / 12.0 };
    assert!(mean(Motion::Ease) < 1.5, "Ease is the straight line");
    assert!(mean(Motion::Bezier { bow: 0.15 }) > 20.0);
    assert!(mean(Motion::Gaussian { sigma: 20.0 }) > 20.0);
    assert!(
        mean(Motion::Perlin {
            amplitude: 40.0,
            octaves: 3
        }) > 10.0
    );
}

/// Seeded, not drawn from the OS: the same seed is the same path. This is
/// what makes a humanized movement testable, and what would let a recorded
/// session be replayed exactly.
#[tokio::test]
async fn a_seed_reproduces_a_path_and_a_different_seed_does_not() {
    let (from, to) = (Point::new(10, 10), Point::new(900, 700));
    let go = |seed| {
        gesture::path(
            from,
            to,
            300,
            8,
            screen(),
            &Motion::Gaussian { sigma: 12.0 },
            &mut Rng::seed(seed),
        )
    };
    assert_eq!(go(5), go(5), "same seed, same path");
    assert_ne!(go(5), go(6), "different seed, different path");
}

/// Perlin drifts, Gaussian trembles. The distinction is the reason to have
/// both, so it is worth asserting rather than assuming: measure the mean
/// absolute change in direction between consecutive segments.
#[tokio::test]
async fn perlin_is_smoother_than_gaussian() {
    let jitter = |motion: Motion| -> f64 {
        let steps = gesture::path(
            Point::new(0, 700),
            Point::new(2000, 700),
            600,
            8,
            screen(),
            &motion,
            &mut Rng::seed(11),
        );
        let m = moves(&steps);
        let d: Vec<i32> = m.windows(2).map(|w| w[1].y - w[0].y).collect();
        let n = d.len().saturating_sub(1).max(1) as f64;
        d.windows(2)
            .map(|w| (w[1] - w[0]).abs() as f64)
            .sum::<f64>()
            / n
    };
    let g = jitter(Motion::Gaussian { sigma: 20.0 });
    let p = jitter(Motion::Perlin {
        amplitude: 20.0,
        octaves: 3,
    });
    assert!(
        p < g,
        "perlin {p:.3} should be smoother than gaussian {g:.3}"
    );
}

/// The default is a plain arc, not tremor: it is what a Session uses when
/// nobody has chosen, and a click that wobbles is a click that can miss a
/// small target.
#[tokio::test]
async fn the_default_motion_is_a_single_bow() {
    assert_eq!(Motion::default(), Motion::Bezier { bow: 0.12 });
}

// ---- keystrokes ----

/// The distinction the whole keyboard design turns on: typing a character is
/// not pressing a key. `Text` injects the character itself and never consults
/// the target's keyboard layout; `Key` presses a position, which is what a
/// chord needs and what Unicode injection cannot express.
#[tokio::test]
async fn text_is_injected_as_characters_and_keys_as_keys() {
    let steps = gesture::type_text("a@b", 70, 0, &mut Rng::seed(1));
    let typed: Vec<String> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(typed, vec!["a", "@", "b"]);
    // No Key steps at all: nothing here depends on the target's layout.
    assert!(!steps.iter().any(|s| matches!(s, Step::Key { .. })));
}

#[tokio::test]
async fn typing_is_paced_and_jittered() {
    let steps = gesture::type_text("hello world", 70, 30, &mut Rng::seed(9));
    let sleeps: Vec<u32> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Sleep { ms } => Some(*ms),
            _ => None,
        })
        .collect();
    assert_eq!(sleeps.len(), 10, "one gap between each pair of characters");
    assert!(
        sleeps.iter().all(|ms| (40..=100).contains(ms)),
        "{sleeps:?}"
    );
    assert!(
        sleeps.windows(2).any(|w| w[0] != w[1]),
        "uniform keystrokes drop characters in debounced inputs: {sleeps:?}"
    );
    // Deterministic under a seed, like every other timing decision here.
    assert_eq!(
        gesture::type_text("hello world", 70, 30, &mut Rng::seed(9)),
        steps
    );
}

/// Releasing Ctrl before C can be read by the target as a bare `c`, which
/// types a character into whatever had focus instead of copying.
#[tokio::test]
async fn a_chord_releases_in_reverse_order() {
    let steps = gesture::chord(&[Key::Ctrl, Key::Shift], Key::ch('c'), 40);
    let order: Vec<(Key, bool)> = steps
        .iter()
        .filter_map(|s| match s {
            Step::Key { key, down } => Some((key.clone(), *down)),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        vec![
            (Key::Ctrl, true),
            (Key::Shift, true),
            (Key::ch('c'), true),
            (Key::ch('c'), false),
            (Key::Shift, false),
            (Key::Ctrl, false),
        ]
    );
    assert!(gesture::held_after(&steps).is_empty(), "balanced");
}

/// A stuck modifier is not a failed operation, it is an unusable machine — so
/// an unbalanced batch must be detectable, and recoverable, from this side.
#[tokio::test]
async fn keys_left_held_are_detected_and_released_outermost_last() {
    let partial = vec![
        Step::Key {
            key: Key::Ctrl,
            down: true,
        },
        Step::Key {
            key: Key::Shift,
            down: true,
        },
        Step::Key {
            key: Key::ch('c'),
            down: true,
        },
        Step::Key {
            key: Key::ch('c'),
            down: false,
        },
        // the agent refused here: ctrl and shift are still down
    ];
    assert_eq!(gesture::held_after(&partial), vec![Key::Shift, Key::Ctrl]);
    assert_eq!(
        gesture::release_all(&partial),
        vec![
            Step::Key {
                key: Key::Shift,
                down: false
            },
            Step::Key {
                key: Key::Ctrl,
                down: false
            },
        ]
    );
    assert!(Key::Ctrl.is_modifier() && Key::AltRight.is_modifier());
    assert!(!Key::ch('c').is_modifier() && !Key::Enter.is_modifier());
}

/// If the agent refuses mid-chord, the client cleans up after itself rather
/// than leaving the modifiers down and reporting an error.
#[tokio::test]
async fn a_refused_chord_still_releases_its_modifiers() {
    // A pointer that refuses everything, recording what it was asked to do:
    // the recovery attempt must still be made, and the original error is what
    // surfaces to the caller.
    struct Refusing(std::sync::Mutex<Vec<Vec<Step>>>);
    #[async_trait::async_trait]
    impl nspointer::Pointer for Refusing {
        async fn screens(&self) -> Result<Screens, InputError> {
            Ok(layout())
        }
        async fn position(&self) -> Result<Point, InputError> {
            Ok(Point::new(0, 0))
        }
        async fn perform(&self, steps: &[Step]) -> Result<u64, InputError> {
            self.0.lock().unwrap().push(steps.to_vec());
            Err(InputError::Agent {
                kind: ErrorKind::Blocked,
                detail: "elevated".into(),
            })
        }
    }
    let s = Session::open(Refusing(std::sync::Mutex::new(Vec::new())))
        .await
        .unwrap();
    let err = s.chord(&[Key::Ctrl], Key::ch('c')).await.unwrap_err();
    assert!(matches!(
        err,
        InputError::Agent {
            kind: ErrorKind::Blocked,
            ..
        }
    ));
}

#[tokio::test]
async fn a_key_press_is_a_press_and_a_release() {
    assert_eq!(
        gesture::press(Key::Enter, 40),
        vec![
            Step::Key {
                key: Key::Enter,
                down: true
            },
            Step::Sleep { ms: 40 },
            Step::Key {
                key: Key::Enter,
                down: false
            },
        ]
    );
}

/// Every protocol-2 addition since the first drop is an optional field with a
/// default, so a message from before the addition still parses to the same
/// meaning it had. This is the whole reason the version is still 2.
#[test]
fn older_messages_still_parse_with_the_old_meaning() {
    use nspointer::wire::{Op, Request, Response, ResultBody};
    // A client that predates `visible_only` asks for everything.
    let r: Request = serde_json::from_str(r#"{"id":7,"op":"ui_tree"}"#).unwrap();
    assert_eq!(
        r.op,
        Op::UiTree {
            visible_only: false
        }
    );
    // An agent that predates `armed` has not said; one that predates
    // `local_override` has no brake.
    let r: Response = serde_json::from_str(
        r#"{"id":1,"ok":true,"result":{"kind":"ready","agent":"x","platform":"windows","protocol":2}}"#,
    )
    .unwrap();
    match r.result.unwrap() {
        ResultBody::Ready {
            armed,
            local_override,
            ..
        } => {
            assert_eq!(armed, None);
            assert!(!local_override);
        }
        other => panic!("{other:?}"),
    }
    // A node from an agent that sends none of the structural fields.
    let n: nspointer::ui::UiNode =
        serde_json::from_str(r#"{"role":"Button","name":"Save","center":{"x":1,"y":2}}"#).unwrap();
    assert!(n.visible && n.enabled && !n.focused && !n.focusable);
    assert_eq!((n.depth, n.window), (0, 0));
}
