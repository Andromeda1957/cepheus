use super::*;
use crate::screen::{M_MOUSE_CLICK, M_SGR_MOUSE};

fn wheel(app: &mut App, kind: MouseEventKind) {
    app.handle_input(Event::Mouse(MouseEvent { kind, column: 40, row: 5, modifiers: KeyModifiers::NONE }));
}

fn distance(rx: &Receiver<ClientMsg>) -> usize {
    rx.try_iter()
        .map(|msg| match msg {
            ClientMsg::Scroll { op: ScrollOp::Lines(lines), .. } => lines.unsigned_abs() as usize,
            ClientMsg::Input { data, .. } => data.bytes().filter(|b| *b == 0x1b).count(),
            other => panic!("unexpected scroll message: {other:?}"),
        })
        .sum()
}

#[test]
fn rapid_trackpad_gestures_accelerate_in_each_terminal_mode_and_reverse_precisely() {
    for modes in [0, M_MOUSE_CLICK | M_SGR_MOUSE, M_ALT_SCREEN | M_ALTERNATE_SCROLL] {
        let (mut app, rx) = tests::app_with(vec![tests::session(1, Tool::Claude, Status::Idle, true)], vec![]);
        app.set_pane(Rect::new(35, 0, 80, 24));
        app.frame = Some((
            1,
            ScreenFrame { epoch: 1, revision: 1, cols: 80, rows: 24, history: 1000, modes, ..ScreenFrame::default() },
        ));
        rx.try_iter().for_each(drop);
        wheel(&mut app, MouseEventKind::ScrollUp);
        if modes == 0 {
            // The ordinary screen's wheel reads local history: one snapshot
            // request, never input to the program or a shared terminal.
            assert!(matches!(rx.try_recv(), Ok(ClientMsg::History { id: 1, .. })));
            for _ in 0..8 {
                wheel(&mut app, MouseEventKind::ScrollUp);
            }
            assert!(rx.try_recv().is_err(), "further steps wait for that snapshot locally");
            continue;
        }
        let precise = distance(&rx);
        assert!(precise > 0);
        for _ in 0..8 {
            wheel(&mut app, MouseEventKind::ScrollUp);
        }
        let fast = distance(&rx);
        assert!(
            fast > 8 * precise,
            "rapid gestures must travel further than isolated steps (modes={modes}, fast={fast}, precise={precise})"
        );
        assert!(fast <= 8 * precise * 4, "acceleration must remain bounded");
        wheel(&mut app, MouseEventKind::ScrollDown);
        assert_eq!(distance(&rx), precise, "reversing direction must start with a precise step");
    }
}

#[test]
fn a_pause_or_target_change_ends_the_fast_gesture() {
    let mut gesture = scroll::ScrollGesture::default();
    let now = Instant::now();
    let precise = gesture.multiplier(1, true, now);
    for n in 1..10 {
        assert!(gesture.multiplier(1, true, now + Duration::from_millis(n * 10)) > precise);
    }
    let later = now + Duration::from_secs(1);
    assert_eq!(gesture.multiplier(1, true, later), precise, "a pause restores precision");
    assert!(gesture.multiplier(1, true, later) > precise);
    assert_eq!(gesture.multiplier(2, true, later), precise, "another session starts precisely");
    assert!(gesture.multiplier(2, true, later) > precise);
    assert_eq!(gesture.multiplier(2, false, later), precise, "direction changes restore precision");
}

#[test]
fn accelerated_mouse_reports_keep_coordinates_and_do_not_amplify_other_input() {
    let (mut app, rx) = tests::app_with(vec![tests::session(1, Tool::Claude, Status::Idle, true)], vec![]);
    app.set_pane(Rect::new(35, 0, 80, 24));
    app.frame = Some((1, ScreenFrame { modes: M_MOUSE_CLICK | M_SGR_MOUSE, ..ScreenFrame::default() }));
    rx.try_iter().for_each(drop);
    for _ in 0..8 {
        wheel(&mut app, MouseEventKind::ScrollUp);
    }
    for msg in rx.try_iter() {
        let ClientMsg::Input { id, data } = msg else { panic!("mouse tracking must receive native reports") };
        assert_eq!(id, 1);
        let one = "\x1b[<64;6;6M";
        assert_eq!(data, one.repeat(data.len() / one.len()), "each report keeps pane-relative coordinates");
    }
    for (kind, modifiers, expected) in [
        (MouseEventKind::ScrollLeft, KeyModifiers::NONE, "\x1b[<66;6;6M"),
        (MouseEventKind::ScrollUp, KeyModifiers::CONTROL, "\x1b[<80;6;6M"),
        (MouseEventKind::Down(MouseButton::Right), KeyModifiers::NONE, "\x1b[<2;6;6M"),
    ] {
        app.handle_input(Event::Mouse(MouseEvent { kind, column: 40, row: 5, modifiers }));
        let ClientMsg::Input { data, .. } = rx.try_recv().unwrap() else { panic!("expected mouse input") };
        assert_eq!(data, expected, "non-gesture input stays a single report");
        assert!(rx.try_recv().is_err());
        wheel(&mut app, MouseEventKind::ScrollUp);
        assert_eq!(distance(&rx), 1, "other input ends acceleration");
        wheel(&mut app, MouseEventKind::ScrollUp);
        assert!(distance(&rx) > 1);
    }
    app.handle_input(Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
    rx.try_iter().for_each(drop);
    wheel(&mut app, MouseEventKind::ScrollUp);
    assert_eq!(distance(&rx), 1, "typing ends acceleration");
}
