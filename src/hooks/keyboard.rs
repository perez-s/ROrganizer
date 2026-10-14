use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, KBDLLHOOKSTRUCT, WM_KEYDOWN, WM_SYSKEYDOWN,
};

use super::state::{BindingAction, HOOK_ENABLED};
use crate::triggers::Trigger;

pub unsafe extern "system" fn kb_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT { unsafe {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    // Hot-path early-out: lock-free atomic check before touching the mutex.
    if !HOOK_ENABLED.load(Ordering::Relaxed) {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let msg = wparam.0 as u32;
    if msg != WM_KEYDOWN && msg != WM_SYSKEYDOWN {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let vk = info.vkCode;

    if super::dispatch(Trigger::Key(vk)) {
        return LRESULT(1);
    }

    CallNextHookEx(None, code, wparam, lparam)
}}

/// Looks up the trigger against the binding table and returns the HWND to
/// focus, advancing the cycler cursor as needed. Caller still holds the lock.
pub(super) fn resolve_action(
    state: &mut super::state::HookState,
    trigger: Trigger,
) -> Option<isize> {
    let action = state
        .bindings
        .iter()
        .find(|(t, _)| *t == trigger)
        .map(|(_, a)| *a)?;
    let target = match action {
        BindingAction::Focus(h) => {
            // A direct focus binding aligns the cycler on its target so the
            // next CycleNext from there continues the user's expected flow.
            if let Some(pos) = state.cycle_hwnds.iter().position(|&x| x == h) {
                state.cycle_index = pos;
            }
            Some(h)
        }
        BindingAction::CycleNext => {
            let len = state.cycle_hwnds.len();
            if len == 0 {
                return None;
            }
            state.cycle_index = (state.cycle_index + 1) % len;
            Some(state.cycle_hwnds[state.cycle_index])
        }
        BindingAction::CyclePrev => {
            let len = state.cycle_hwnds.len();
            if len == 0 {
                return None;
            }
            state.cycle_index = (state.cycle_index + len - 1) % len;
            Some(state.cycle_hwnds[state.cycle_index])
        }
        // Macros don't focus anything here — see `resolve_macro`.
        BindingAction::ClickAll | BindingAction::InviteAll => None,
    };
    if let Some(h) = target {
        state.cycle_current_hwnd = Some(h);
    }
    target
}

/// Returns the macro bound to `trigger`, if the first matching binding is
/// one. Same first-match rule as `resolve_action`, so an Account binding
/// pushed earlier still wins on a conflicting trigger.
pub(super) fn resolve_macro(
    state: &super::state::HookState,
    trigger: Trigger,
) -> Option<crate::macros::Job> {
    let (_, action) = state.bindings.iter().find(|(t, _)| *t == trigger)?;
    match action {
        BindingAction::ClickAll if !state.cycle_hwnds.is_empty() => {
            Some(crate::macros::Job::ClickAll(state.cycle_hwnds.clone()))
        }
        // The leader is the first account in cycle order; it needs at least
        // one other account to invite.
        BindingAction::InviteAll if state.cycle_hwnds.len() >= 2 => {
            Some(crate::macros::Job::InviteAll {
                leader: state.cycle_hwnds[0],
                names: state.cycle_names.get(1..)?.to_vec(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::triggers::Trigger;

    fn make_state(
        bindings: Vec<(Trigger, BindingAction)>,
        cycle_hwnds: Vec<isize>,
    ) -> super::super::state::HookState {
        super::super::state::HookState {
            bindings,
            cycle_names: cycle_hwnds.iter().map(|h| format!("p{h}")).collect(),
            cycle_hwnds,
            cycle_index: 0,
            cycle_current_hwnd: None,
        }
    }

    #[test]
    fn empty_bindings_resolves_to_none() {
        let mut st = make_state(Vec::new(), vec![100, 200]);
        assert!(resolve_action(&mut st, Trigger::Key(0x70)).is_none());
    }

    #[test]
    fn unmatched_trigger_resolves_to_none() {
        let mut st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::Focus(100))],
            vec![100],
        );
        assert!(resolve_action(&mut st, Trigger::Key(0x71)).is_none());
    }

    #[test]
    fn focus_aligns_cycle_index_to_target() {
        let mut st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::Focus(300))],
            vec![100, 200, 300, 400],
        );
        st.cycle_index = 0; // start away from 300
        let r = resolve_action(&mut st, Trigger::Key(0x70));
        assert_eq!(r, Some(300));
        assert_eq!(st.cycle_index, 2);
        assert_eq!(st.cycle_current_hwnd, Some(300));
    }

    #[test]
    fn focus_does_not_move_cursor_if_target_not_in_cycle() {
        let mut st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::Focus(999))],
            vec![100, 200],
        );
        st.cycle_index = 1;
        resolve_action(&mut st, Trigger::Key(0x70));
        assert_eq!(st.cycle_index, 1, "untouched when target outside cycle");
        assert_eq!(st.cycle_current_hwnd, Some(999));
    }

    #[test]
    fn cycle_next_wraps_last_to_first() {
        let mut st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::CycleNext)],
            vec![100, 200, 300],
        );
        st.cycle_index = 2;
        let r = resolve_action(&mut st, Trigger::Key(0x70));
        assert_eq!(r, Some(100));
        assert_eq!(st.cycle_index, 0);
        assert_eq!(st.cycle_current_hwnd, Some(100));
    }

    #[test]
    fn cycle_next_advances_when_not_at_end() {
        let mut st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::CycleNext)],
            vec![100, 200, 300],
        );
        st.cycle_index = 0;
        let r = resolve_action(&mut st, Trigger::Key(0x70));
        assert_eq!(r, Some(200));
        assert_eq!(st.cycle_index, 1);
    }

    #[test]
    fn cycle_prev_wraps_first_to_last() {
        let mut st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::CyclePrev)],
            vec![100, 200, 300],
        );
        st.cycle_index = 0;
        let r = resolve_action(&mut st, Trigger::Key(0x70));
        assert_eq!(r, Some(300));
        assert_eq!(st.cycle_index, 2);
    }

    #[test]
    fn cycle_prev_decrements_when_not_at_start() {
        let mut st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::CyclePrev)],
            vec![100, 200, 300],
        );
        st.cycle_index = 2;
        let r = resolve_action(&mut st, Trigger::Key(0x70));
        assert_eq!(r, Some(200));
        assert_eq!(st.cycle_index, 1);
    }

    #[test]
    fn cycle_next_on_empty_list_returns_none() {
        let mut st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::CycleNext)],
            Vec::new(),
        );
        assert!(resolve_action(&mut st, Trigger::Key(0x70)).is_none());
        assert_eq!(st.cycle_current_hwnd, None);
    }

    #[test]
    fn cycle_prev_on_empty_list_returns_none() {
        let mut st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::CyclePrev)],
            Vec::new(),
        );
        assert!(resolve_action(&mut st, Trigger::Key(0x70)).is_none());
    }

    #[test]
    fn click_all_resolves_to_macro_with_cycle_order() {
        let st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::ClickAll)],
            vec![100, 200, 300],
        );
        assert_eq!(
            resolve_macro(&st, Trigger::Key(0x70)),
            Some(crate::macros::Job::ClickAll(vec![100, 200, 300]))
        );
    }

    #[test]
    fn click_all_is_not_a_focus_action() {
        let mut st = make_state(
            vec![(Trigger::Key(0x70), BindingAction::ClickAll)],
            vec![100, 200],
        );
        assert!(resolve_action(&mut st, Trigger::Key(0x70)).is_none());
        assert_eq!(st.cycle_current_hwnd, None);
    }

    #[test]
    fn click_all_without_live_accounts_does_nothing() {
        let st = make_state(vec![(Trigger::Key(0x70), BindingAction::ClickAll)], Vec::new());
        assert!(resolve_macro(&st, Trigger::Key(0x70)).is_none());
    }

    #[test]
    fn invite_all_targets_leader_with_the_other_names() {
        let st = make_state(
            vec![(Trigger::Key(0x71), BindingAction::InviteAll)],
            vec![100, 200, 300],
        );
        assert_eq!(
            resolve_macro(&st, Trigger::Key(0x71)),
            Some(crate::macros::Job::InviteAll {
                leader: 100,
                names: vec!["p200".into(), "p300".into()],
            })
        );
    }

    #[test]
    fn invite_all_needs_a_second_account() {
        let st = make_state(vec![(Trigger::Key(0x71), BindingAction::InviteAll)], vec![100]);
        assert!(resolve_macro(&st, Trigger::Key(0x71)).is_none());
    }

    #[test]
    fn focus_binding_shadows_click_all_on_same_trigger() {
        let st = make_state(
            vec![
                (Trigger::Key(0x70), BindingAction::Focus(500)),
                (Trigger::Key(0x70), BindingAction::ClickAll),
            ],
            vec![100, 200],
        );
        assert!(resolve_macro(&st, Trigger::Key(0x70)).is_none());
    }

    #[test]
    fn first_matching_binding_wins_on_duplicate_trigger() {
        // App pushes Account focus actions BEFORE Cycle ones, so Account wins.
        let mut st = make_state(
            vec![
                (Trigger::Key(0x70), BindingAction::Focus(500)),
                (Trigger::Key(0x70), BindingAction::CycleNext),
            ],
            vec![100, 200],
        );
        let r = resolve_action(&mut st, Trigger::Key(0x70));
        assert_eq!(r, Some(500));
    }
}
