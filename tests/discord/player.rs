use super::*;

#[test]
fn buttons_read_back_as_their_actions() {
    for button in &BUTTONS {
        assert_eq!(button_action(button.custom_id), Some(button.action));
    }
    assert_eq!(button_action("player:destroy"), None);
    assert_eq!(button_action("player:shuffle"), None);
    assert_eq!(button_action("player:add"), Some(Action::Add));
    assert_eq!(
        button_action("player:stop"),
        Some(Action::Control(Control::Stop))
    );
}
