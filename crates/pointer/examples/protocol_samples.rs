//! Prints one real sample of every message. `docs/pointer-protocol.md` is
//! pasted from this output, so the contract cannot drift from the types.
use nspointer::geom::*;
use nspointer::wire::*;

fn line(label: &str, v: &impl serde::Serialize) {
    println!("{label}\n{}\n", serde_json::to_string(v).unwrap());
}

fn main() {
    line(
        "REQ hello",
        &Request {
            id: 1,
            op: Op::Hello {
                token: "<shared secret>".into(),
                protocol: PROTOCOL,
            },
        },
    );
    line(
        "REQ screens",
        &Request {
            id: 2,
            op: Op::Screens,
        },
    );
    line(
        "REQ position",
        &Request {
            id: 3,
            op: Op::Position,
        },
    );
    line(
        "REQ perform",
        &Request {
            id: 4,
            op: Op::Perform {
                steps: vec![
                    Step::Move { x: 1279, y: 719 },
                    Step::Sleep { ms: 8 },
                    Step::Move { x: 1280, y: 720 },
                    Step::Button {
                        button: Button::Left,
                        down: true,
                    },
                    Step::Sleep { ms: 40 },
                    Step::Button {
                        button: Button::Left,
                        down: false,
                    },
                    Step::Scroll { dx: 0, dy: -3 },
                ],
            },
        },
    );
    line(
        "RES ready",
        &Response::ok(
            1,
            ResultBody::Ready {
                agent: "ns-pointerd 0.1.0".into(),
                platform: "windows".into(),
                protocol: PROTOCOL,
                local_override: true,
            },
        ),
    );
    line(
        "RES screens",
        &Response::ok(
            2,
            ResultBody::Screens {
                screens: vec![
                    Screen {
                        id: ScreenId("PRIMARY-EDID-A1".into()),
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
                        id: ScreenId("LEFT-EDID-B2".into()),
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
            },
        ),
    );
    line(
        "RES position",
        &Response::ok(
            3,
            ResultBody::Position {
                x: 1280,
                y: 720,
                state: 7,
            },
        ),
    );
    line(
        "RES performed",
        &Response::ok(4, ResultBody::Performed { steps: 7, state: 7 }),
    );
    line(
        "RES error",
        &Response::err(4, ErrorKind::Blocked, "target window is elevated (UIPI)"),
    );
}
