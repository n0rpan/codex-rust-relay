//! The "Continue in…" sheet's draft and checks. The sheet itself is drawn by `worker_sheet`,
//! which shares its runtime, model and permission fields with the worker sheet.
use super::worker_sheet::{Choice, model_or_none, yolo_confirmed};
use gpui::Entity;
use gpui_component::input::TextareaState;
use ox_desk_core::handoff::HandoffStart;

pub(super) struct HandoffDraft {
    pub choice: Choice,
    pub request: Entity<TextareaState>,
}

/// Builds the start the sheet sends. It applies the same checks as `/handoff`.
pub(super) fn handoff_start(choice: &Choice, request: &str) -> Result<HandoffStart, String> {
    let yolo = yolo_confirmed(choice.permission);
    let start = HandoffStart {
        source: choice.owner.clone(),
        runtime: choice.runtime,
        model: model_or_none(choice.model.as_deref()),
        mode: choice.permission,
        request: request.trim().to_owned(),
        yolo_confirmed: yolo,
    };
    start.validate()?;
    Ok(start)
}

#[cfg(test)]
mod tests {
    use super::handoff_start;
    use crate::views::worker_sheet::Choice;
    use ox_desk_core::modes::{Level, ModeRequest};
    use ox_desk_core::runtime::Runtime;

    fn choice(runtime: Runtime, model: Option<&str>, permission: ModeRequest) -> Choice {
        Choice {
            owner: "s1".into(),
            runtime,
            model: model.map(str::to_owned),
            permission,
            error: None,
        }
    }

    #[test]
    fn empty_request_is_refused() {
        for request in ["", "  \n"] {
            let error = handoff_start(&choice(Runtime::Codex, None, ModeRequest::Inherit), request)
                .unwrap_err();
            assert_eq!(error, "Write the request for the new session.");
        }
    }

    #[test]
    fn request_is_trimmed_and_subscription_is_allowed() {
        let start = handoff_start(
            &choice(Runtime::Claude, Some(" "), ModeRequest::Level(Level::Read)),
            "  summarise the diff  ",
        )
        .unwrap();
        assert_eq!(start.request, "summarise the diff");
        assert_eq!(start.source, "s1");
        assert_eq!(start.model, None);
        assert_eq!(start.mode, ModeRequest::Level(Level::Read));
        assert!(!start.yolo_confirmed);
    }

    #[test]
    fn yolo_handoff_records_the_confirmation() {
        let start = handoff_start(
            &choice(
                Runtime::Codex,
                Some("alias"),
                ModeRequest::Level(Level::Yolo),
            ),
            "go",
        )
        .unwrap();
        assert!(start.yolo_confirmed);
        assert_eq!(start.model.as_deref(), Some("alias"));
    }
}
