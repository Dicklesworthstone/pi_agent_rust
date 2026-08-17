//! Interactive preview of the `FrankenTUI` migration stack (`bd-cv653.9.1`).
//!
//! Runs [`pi::interactive_ftui::PiFtuiModel`] on the real ftui runtime with a
//! scripted fake agent, so the ported surfaces (layout regions, tail-follow
//! scroll, `TextArea` editor, submit round-trip, status chrome, sanitize) can
//! be exercised on an actual terminal before the launch-path integration
//! lands.
//!
//! Run with:
//! ```sh
//! cargo run --example ftui_preview --features ftui
//! ```
//! Type a message and press Enter; the fake agent echoes a streamed reply and
//! demos a tool-status line. Ctrl+C quits.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use pi::interactive::PiMsg;
use pi::interactive_ftui::{PiFtuiModel, UiCommand};
use pi::model::StopReason;

fn main() -> std::io::Result<()> {
    // UI → agent (submitted commands) and agent → UI (PiMsg events).
    let (submit_tx, submit_rx) = mpsc::channel::<UiCommand>();
    let (agent_tx, agent_rx) = mpsc::channel::<PiMsg>();

    // Scripted fake agent: one streamed reply per submitted prompt.
    thread::spawn(move || {
        for command in submit_rx {
            let prompt = match command {
                UiCommand::Prompt(prompt) => prompt,
                UiCommand::SetModel { provider, model } => {
                    let _ = agent_tx.send(PiMsg::System(format!(
                        "(fake agent) pretending to switch to {provider}/{model}"
                    )));
                    continue;
                }
                UiCommand::Bash { command } => {
                    let _ =
                        agent_tx.send(PiMsg::System(format!("(fake agent) would run: {command}")));
                    continue;
                }
            };
            if agent_tx.send(PiMsg::AgentStart).is_err() {
                return;
            }
            let _ = agent_tx.send(PiMsg::ToolStart {
                name: "demo".into(),
                tool_id: "t1".into(),
            });
            thread::sleep(Duration::from_millis(350));
            let _ = agent_tx.send(PiMsg::ToolEnd {
                name: "demo".into(),
                tool_id: "t1".into(),
                is_error: false,
            });
            let reply = format!(
                "You said: {prompt}\nThis reply streams word by word over the \
                 AgentEventSubscription bridge to prove the runtime loop."
            );
            for word in reply.split_inclusive(' ') {
                if agent_tx.send(PiMsg::TextDelta(word.to_string())).is_err() {
                    return;
                }
                thread::sleep(Duration::from_millis(40));
            }
            let _ = agent_tx.send(PiMsg::AgentDone {
                usage: None,
                stop_reason: StopReason::Stop,
                error_message: None,
            });
        }
    });

    let model = PiFtuiModel::new(agent_rx)
        .with_submit_channel(submit_tx)
        .with_palette(pi::interactive_ftui::FtuiPalette::from_theme(
            &pi::theme::Theme::dark(),
        ));
    ftui::App::fullscreen(model).with_mouse().run()
}
