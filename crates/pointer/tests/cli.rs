//! The shell surface: parsing, and what each command does to the wire.

use nspointer::cli::{parse, run, Cmd};
use nspointer::mock::MockPointer;
use nspointer::wire::{Button, Key, Step};
use nspointer::{Point, Rect, Screen, ScreenId, Screens, Session};
use std::sync::Arc;

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn layout() -> Screens {
    Screens {
        screens: vec![
            Screen {
                id: ScreenId::from("PRIMARY"),
                bounds: Rect {
                    x: 0,
                    y: 0,
                    w: 1920,
                    h: 1080,
                },
                scale: 1.0,
                primary: true,
                label: "main".into(),
            },
            Screen {
                id: ScreenId::from("HP"),
                // Taller than the primary, like the machine this was verified
                // on: the second monitor is where the coordinate maths gets
                // interesting.
                bounds: Rect {
                    x: 1920,
                    y: 0,
                    w: 1920,
                    h: 1200,
                },
                scale: 1.0,
                primary: false,
                label: "hp".into(),
            },
        ],
        state: 1,
    }
}

async fn session() -> (Session<Arc<MockPointer>>, Arc<MockPointer>) {
    let m = Arc::new(MockPointer::new(layout(), Point::new(0, 0)));
    (Session::open(m.clone()).await.unwrap(), m)
}

#[tokio::test]
async fn commands_parse_the_way_the_usage_advertises() {
    assert_eq!(parse(&args("screens")).unwrap(), Cmd::Screens);
    assert_eq!(
        parse(&args("move 100 200")).unwrap(),
        Cmd::Move { x: 100, y: 200 }
    );
    assert_eq!(
        parse(&args("click")).unwrap(),
        Cmd::Click {
            at: None,
            button: Button::Left,
            count: 1
        }
    );
    assert_eq!(
        parse(&args("click 5 6 --right --double")).unwrap(),
        Cmd::Click {
            at: Some((5, 6)),
            button: Button::Right,
            count: 2
        }
    );
    assert_eq!(
        parse(&args("key ctrl+shift+s")).unwrap(),
        Cmd::Key {
            mods: vec![Key::Ctrl, Key::Shift],
            key: Key::ch('s')
        }
    );
    assert_eq!(
        parse(&args("key f5")).unwrap(),
        Cmd::Key {
            mods: vec![],
            key: Key::f(5)
        }
    );
    // A sentence needs no quoting: everything after the verb is the text.
    assert_eq!(
        parse(&args("type hello there world")).unwrap(),
        Cmd::Type("hello there world".into())
    );
    assert_eq!(parse(&args("clip")).unwrap(), Cmd::ClipRead);
    assert_eq!(
        parse(&args("clip some text")).unwrap(),
        Cmd::ClipWrite("some text".into())
    );
    assert_eq!(parse(&args("ui")).unwrap(), Cmd::Ui(None));
    assert_eq!(
        parse(&args("ui Save")).unwrap(),
        Cmd::Ui(Some("Save".into()))
    );
}

/// Half a coordinate is a typo. Falling back to "wherever the pointer is"
/// because one number was dropped is how a click lands on something.
#[tokio::test]
async fn a_half_written_click_is_refused_rather_than_reinterpreted() {
    let e = parse(&args("click 5")).unwrap_err();
    assert!(e.contains("both X and Y, or neither"), "{e}");

    for bad in [
        "move 1",
        "move a b",
        "drag 1 2 3",
        "key",
        "key ctrl+",
        "type",
        "find",
        "wat",
    ] {
        assert!(parse(&args(bad)).is_err(), "{bad:?} should not parse");
    }
    assert!(
        parse(&args("key ctrl+enter+c")).is_err(),
        "enter is not a modifier"
    );
}

#[tokio::test]
async fn move_and_click_reach_the_wire_at_the_given_pixels() {
    let (s, m) = session().await;
    // Past 1920: the second monitor, the case that proved virtual-desktop
    // normalization on real Windows.
    let out = run(&s, parse(&args("move 2032 1126")).unwrap())
        .await
        .unwrap();
    assert_eq!(out, "moved to 2032 1126");
    assert_eq!(*m.track().last().unwrap(), Point::new(2032, 1126));

    run(&s, parse(&args("click 800 400 --double")).unwrap())
        .await
        .unwrap();
    let downs = m
        .steps()
        .iter()
        .filter(|x| matches!(x, Step::Button { down: true, .. }))
        .count();
    assert_eq!(downs, 2, "double");
}

/// The count pair is the point: they differ exactly when a surrogate pair is
/// involved, which is the case worth seeing in the output.
#[tokio::test]
async fn typing_reports_both_counts_so_a_surrogate_pair_is_visible() {
    let (s, m) = session().await;
    let out = run(&s, Cmd::Type("Příliš žluťoučký kůň 🐎".into()))
        .await
        .unwrap();
    assert_eq!(out, "typed 22 chars (23 UTF-16 units)");

    let typed: String = m
        .steps()
        .iter()
        .filter_map(|x| match x {
            Step::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(typed, "Příliš žluťoučký kůň 🐎");
}

#[tokio::test]
async fn a_chord_holds_and_releases_in_reverse() {
    let (s, m) = session().await;
    run(&s, parse(&args("key ctrl+c")).unwrap()).await.unwrap();
    let keys: Vec<(Key, bool)> = m
        .steps()
        .iter()
        .filter_map(|x| match x {
            Step::Key { key, down } => Some((key.clone(), *down)),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            (Key::Ctrl, true),
            (Key::ch('c'), true),
            (Key::ch('c'), false),
            (Key::Ctrl, false),
        ]
    );
}

#[tokio::test]
async fn screens_and_clipboard_render_for_a_person() {
    let (s, _) = session().await;
    let out = run(&s, Cmd::Screens).await.unwrap();
    assert!(out.contains("PRIMARY"));
    assert!(
        out.contains("1920x1200 at (1920,0)"),
        "second monitor offset: {out}"
    );
    assert!(out.contains("primary"));

    run(&s, Cmd::ClipWrite("hunter2".into())).await.unwrap();
    assert_eq!(run(&s, Cmd::ClipRead).await.unwrap(), "hunter2");
}

/// An agent without `ui_tree` must say so rather than look empty.
#[tokio::test]
async fn a_missing_capability_surfaces_as_an_error_not_an_empty_result() {
    let (s, _) = session().await;
    let e = run(&s, Cmd::Find("Save".into())).await.unwrap_err();
    assert!(e.to_string().contains("Unsupported"), "{e}");
}
