#!/usr/bin/env python3
import sys

with open("crates/harnx-tui/src/input.rs", "r") as f:
    content = f.read()

old_run_command = """
        match result {
            Ok(outcome) => {
                self.maybe_open_picker_after_command(outcome, prev_agent.clone())
                    .await;
                let llm_busy = self.app.llm_busy;
                let pending_message = self.app.pending_message.is_some();
                Self::refresh_input_chrome_from_state(
                    &self.config,
                    &mut self.app,
                    llm_busy,
                    pending_message,
                );
                self.reconcile_transcript_after_command(prev_session, prev_agent, line);
                if !clean.is_empty() {
                    if is_mutation_command {
                        self.app
                            .transcript
                            .push(TranscriptItem::MutationNotice(clean.clone()));
                    } else {
                        self.app
                            .transcript
                            .push(TranscriptItem::SystemText(clean.clone()));
                    }
                    self.pin_transcript_to_bottom();
                }
            }
            Err(err) => {
                self.app
                    .transcript
                    .push(TranscriptItem::ErrorText(pretty_error_string(&err)));
            }
        }
        Ok(())
    }
"""

new_run_command = """
        self.finish_command(
            result,
            clean,
            line,
            prev_session,
            prev_agent,
            is_mutation_command,
        )
        .await;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_command(
        &mut self,
        result: Result<harnx_runtime::commands::CommandOutcome>,
        clean: String,
        line: &str,
        prev_session: Option<String>,
        prev_agent: Option<String>,
        is_mutation_command: bool,
    ) {
        match result {
            Ok(outcome) => {
                self.maybe_open_picker_after_command(outcome, prev_agent.clone())
                    .await;
                let llm_busy = self.app.llm_busy;
                let pending_message = self.app.pending_message.is_some();
                Self::refresh_input_chrome_from_state(
                    &self.config,
                    &mut self.app,
                    llm_busy,
                    pending_message,
                );
                self.reconcile_transcript_after_command(prev_session, prev_agent, line);
                if !clean.is_empty() {
                    if is_mutation_command {
                        self.app
                            .transcript
                            .push(TranscriptItem::MutationNotice(clean.clone()));
                    } else {
                        self.app
                            .transcript
                            .push(TranscriptItem::SystemText(clean.clone()));
                    }
                    self.pin_transcript_to_bottom();
                }
            }
            Err(err) => {
                self.app
                    .transcript
                    .push(TranscriptItem::ErrorText(pretty_error_string(&err)));
            }
        }
    }
"""

if old_run_command not in content:
    print("Could not find block to replace.")
    sys.exit(1)

new_content = content.replace(old_run_command, new_run_command)
with open("crates/harnx-tui/src/input.rs", "w") as f:
    f.write(new_content)
print("Updated successfully.")
