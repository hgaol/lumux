use super::*;
use crate::traits::PtySize;

fn sh() -> Vec<String> {
    vec!["/bin/sh".to_string()]
}

/// Invariant the model must always uphold: every window has >=1 pane and every
/// session has >=1 window. Empty containers must have been cascade-closed.
fn assert_no_empty_containers(server: &Server) {
    for sid in server.session_ids() {
        let s = server.session(sid).unwrap();
        assert!(s.window_count() >= 1, "session {sid} has no windows");
        for wid in s.window_ids() {
            assert!(
                s.window(wid).unwrap().pane_count() >= 1,
                "window {wid} has no panes"
            );
        }
    }
}

#[test]
fn new_session_has_one_window_one_pane() {
    let mut srv = Server::new();
    let sid = srv.new_session("work", sh());
    let s = srv.session(sid).unwrap();
    assert_eq!(s.window_count(), 1);
    assert_eq!(s.window(s.active_window()).unwrap().pane_count(), 1);
    assert_eq!(s.name, "work");
    assert_no_empty_containers(&srv);
}

#[test]
fn find_session_by_name() {
    let mut srv = Server::new();
    let sid = srv.new_session("alpha", sh());
    assert_eq!(srv.find_session_by_name("alpha"), Some(sid));
    assert_eq!(srv.find_session_by_name("missing"), None);
}

#[test]
fn split_adds_pane_and_focuses_it() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    let new = srv.split_active(sid, sh(), SplitDir::Horizontal).unwrap();
    let w = srv.session(sid).unwrap();
    let win = w.window(w.active_window()).unwrap();
    assert_eq!(win.pane_count(), 2);
    assert_eq!(win.active_pane(), new, "new pane should take focus");
}

#[test]
fn kill_one_of_two_panes_keeps_window() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    let new = srv.split_active(sid, sh(), SplitDir::Vertical).unwrap();
    assert_eq!(srv.kill_pane(sid, new), CascadeResult::PaneClosed);
    let w = srv.session(sid).unwrap();
    assert_eq!(w.window(w.active_window()).unwrap().pane_count(), 1);
    assert_no_empty_containers(&srv);
}

#[test]
fn kill_last_pane_cascades_to_session_close() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    let pane = srv.session(sid).unwrap();
    let only_pane = pane
        .window(pane.active_window())
        .unwrap()
        .active_pane();
    assert_eq!(srv.kill_pane(sid, only_pane), CascadeResult::SessionClosed);
    assert!(srv.session(sid).is_none());
    assert!(srv.is_empty());
}

#[test]
fn kill_last_pane_of_window_closes_window_not_session() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    srv.new_window(sid, "second", sh());
    // Session now has 2 windows. Kill the sole pane of the active (second) one.
    let s = srv.session(sid).unwrap();
    let active = s.active_window();
    let pane = s.window(active).unwrap().active_pane();
    assert_eq!(srv.kill_pane(sid, pane), CascadeResult::WindowClosed);
    let s = srv.session(sid).unwrap();
    assert_eq!(s.window_count(), 1);
    assert_no_empty_containers(&srv);
}

#[test]
fn window_navigation_wraps() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    let w0 = srv.session(sid).unwrap().active_window();
    let w1 = srv.new_window(sid, "1", sh()).unwrap();
    let w2 = srv.new_window(sid, "2", sh()).unwrap();
    let s = srv.session_mut(sid).unwrap();
    // Currently active = w2 (last added).
    assert_eq!(s.active_window(), w2);
    s.focus_next_window();
    assert_eq!(s.active_window(), w0, "next from last wraps to first");
    s.focus_prev_window();
    assert_eq!(s.active_window(), w2, "prev from first wraps to last");
    assert_eq!(s.window_ids(), vec![w0, w1, w2]);
}

#[test]
fn pane_focus_navigation_wraps() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    let p0 = srv.session(sid).unwrap().window(srv.session(sid).unwrap().active_window()).unwrap().active_pane();
    let p1 = srv.split_active(sid, sh(), SplitDir::Horizontal).unwrap();
    let win = srv.session_mut(sid).unwrap().active_window_mut();
    assert_eq!(win.active_pane(), p1);
    win.focus_next_pane();
    assert_eq!(win.active_pane(), p0, "wraps back to first pane");
}

#[test]
fn killing_focused_pane_refocuses_survivor() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    let p1 = srv.split_active(sid, sh(), SplitDir::Horizontal).unwrap();
    // p1 is focused; kill it, focus must move to the survivor.
    srv.kill_pane(sid, p1);
    let win = srv.session(sid).unwrap();
    let win = win.window(win.active_window()).unwrap();
    assert_eq!(win.pane_count(), 1);
    assert_ne!(win.active_pane(), p1);
}

#[test]
fn ids_remain_stable_across_unrelated_kills() {
    let mut srv = Server::new();
    let a = srv.new_session("a", sh());
    let b = srv.new_session("b", sh());
    let b_pane = {
        let s = srv.session(b).unwrap();
        s.window(s.active_window()).unwrap().active_pane()
    };
    srv.kill_session(a);
    // b and its pane id are untouched.
    assert!(srv.session(b).is_some());
    let s = srv.session(b).unwrap();
    assert_eq!(s.window(s.active_window()).unwrap().active_pane(), b_pane);
}

#[test]
fn cascade_on_missing_pane_is_not_found() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    assert_eq!(srv.kill_pane(sid, PaneId(9999)), CascadeResult::NotFound);
    assert_eq!(
        srv.kill_pane(SessionId(9999), PaneId(1)),
        CascadeResult::NotFound
    );
}

// --- client registry / sizing ---

#[test]
fn attach_detach_clients() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    let c1 = srv.attach_client(sid, PtySize::new(80, 24)).unwrap();
    let c2 = srv.attach_client(sid, PtySize::new(100, 30)).unwrap();
    assert_eq!(srv.client_count(), 2);
    assert_eq!(srv.clients_of(sid).len(), 2);
    assert!(srv.detach_client(c1));
    assert_eq!(srv.client_count(), 1);
    assert!(!srv.detach_client(c1), "double-detach is false");
    assert!(srv.detach_client(c2));
}

#[test]
fn attach_to_missing_session_fails() {
    let mut srv = Server::new();
    assert!(srv.attach_client(SessionId(123), PtySize::default()).is_none());
}

#[test]
fn smallest_client_wins_sizing() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    assert_eq!(srv.effective_size(sid), None, "no clients => no size");
    srv.attach_client(sid, PtySize::new(120, 40)).unwrap();
    srv.attach_client(sid, PtySize::new(80, 50)).unwrap();
    srv.attach_client(sid, PtySize::new(100, 24)).unwrap();
    // min cols=80, min rows=24.
    assert_eq!(srv.effective_size(sid), Some(PtySize::new(80, 24)));
}

#[test]
fn killing_session_drops_its_clients() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    srv.attach_client(sid, PtySize::default()).unwrap();
    assert_eq!(srv.client_count(), 1);
    srv.kill_session(sid);
    assert_eq!(srv.client_count(), 0, "clients of a killed session are dropped");
}

/// Pseudo-property test: a randomized-ish sequence of splits and kills never
/// leaves an empty window or session, and the session closes exactly when its
/// last pane dies.
#[test]
fn invariant_holds_across_op_sequence() {
    let mut srv = Server::new();
    let sid = srv.new_session("s", sh());
    let mut live: Vec<PaneId> = {
        let s = srv.session(sid).unwrap();
        vec![s.window(s.active_window()).unwrap().active_pane()]
    };
    // Grow.
    for i in 0..8 {
        let dir = if i % 2 == 0 {
            SplitDir::Horizontal
        } else {
            SplitDir::Vertical
        };
        if i % 3 == 0 {
            if let Some(w) = srv.new_window(sid, format!("w{i}"), sh()) {
                let p = srv.session(sid).unwrap().window(w).unwrap().active_pane();
                live.push(p);
            }
        } else if let Some(p) = srv.split_active(sid, sh(), dir) {
            live.push(p);
        }
        assert_no_empty_containers(&srv);
    }
    // Shrink: kill all but watch for the cascade close.
    let mut closed = false;
    for p in live {
        match srv.kill_pane(sid, p) {
            CascadeResult::SessionClosed => {
                closed = true;
                break;
            }
            CascadeResult::NotFound => {} // already collapsed away
            _ => assert_no_empty_containers(&srv),
        }
    }
    assert!(closed, "session must close once its last pane is killed");
    assert!(srv.is_empty());
}
