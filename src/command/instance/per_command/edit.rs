use crate::command::commands::edit::EditCommandFrontend;
use crate::command::instance::frontend::InstanceCommandFrontend;

impl EditCommandFrontend for InstanceCommandFrontend {
    fn show_edit_outcome(&mut self, outcome: &crate::command::commands::edit::EditOutcome) {
        use crate::data::message::{MessageLevel, UserMessage, UserMessageSink};
        for line in outcome.render().lines() {
            self.write_message(UserMessage {
                level: MessageLevel::Info,
                text: line.to_string(),
            });
        }
    }
}
