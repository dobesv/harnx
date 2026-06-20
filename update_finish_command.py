#!/usr/bin/env python3
import sys

with open("crates/harnx-tui/src/input.rs", "r") as f:
    content = f.read()

old_finish = """    #[allow(clippy::too_many_arguments)]
    async fn finish_command(
        &mut self,
        result: Result<harnx_runtime::commands::CommandOutcome>,
        clean: String,
        line: &str,
        prev_session: Option<String>,
        prev_agent: Option<String>,
        is_mutation_command: bool,
    ) {"""

new_finish = """    async fn finish_command(
        &mut self,
        result: Result<harnx_runtime::commands::CommandOutcome>,
        clean: String,
        ctx: (&str, Option<String>, Option<String>, bool),
    ) {
        let (line, prev_session, prev_agent, is_mutation_command) = ctx;"""

if old_finish not in content:
    print("Could not find old finish_command signature.")
    sys.exit(1)

content = content.replace(old_finish, new_finish)

old_call = """        self.finish_command(
            result,
            clean,
            line,
            prev_session,
            prev_agent,
            is_mutation_command,
        )
        .await;"""

new_call = """        self.finish_command(
            result,
            clean,
            (line, prev_session, prev_agent, is_mutation_command),
        )
        .await;"""

if old_call not in content:
    print("Could not find old finish_command call.")
    sys.exit(1)

content = content.replace(old_call, new_call)

with open("crates/harnx-tui/src/input.rs", "w") as f:
    f.write(content)
print("Updated successfully.")
