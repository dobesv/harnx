use super::*;
use crate::user_principals::{anonymous, from_resolved};

const USER_TEMPLATE: &str = "{{ __user__.user_id }}|{{ __user__.users | join(',') }}|\
    {{ __user__.groups | join(',') }}|{{ __user__.roles | join(',') }}|\
    {% if 'developers' in __user__.groups %}developer{% endif %}";

fn developer_agent() -> AgentConfig {
    let mut agent = AgentConfig::from_prompt(USER_TEMPLATE);
    agent.set_user_principals(Some(from_resolved(
        Some("alice"),
        Some(&["alice".into(), "engineering-alias".into()]),
        &["developers".into(), "engineering".into()],
        &["reviewer".into()],
    )));
    agent
}

#[test]
fn user_snapshot_is_available_in_all_instruction_renderers() {
    let agent = developer_agent();
    let expected = "alice|alice,engineering-alias|developers,engineering|reviewer|developer";
    assert_eq!(agent.system_text().unwrap(), expected);
    assert_eq!(agent.system_text_with_tools(&[]).unwrap(), expected);
    assert_eq!(agent.interpolated_instructions().unwrap(), expected);
    assert_eq!(
        render_template("{{ __user__ | list | sort | join(',') }}", &agent, None).unwrap(),
        "groups,roles,user_id,users",
        "the template namespace exposes exactly four keys"
    );
}

#[test]
fn user_without_snapshot_has_defined_anonymous_defaults() {
    let mut agent = AgentConfig::from_prompt(USER_TEMPLATE);
    // Direct access to every field and group membership must work in strict mode.
    let rendered = agent.system_text().unwrap();
    assert_eq!(rendered, "None||||");
    assert_eq!(
        render_template(
            "{{ __user__.user_id is none }}|{{ __user__.users == [] }}|\
             {{ __user__.groups == [] }}|{{ __user__.roles == [] }}|{{ __user__ | length }}",
            &agent,
            None,
        )
        .unwrap(),
        "True|True|True|True|4"
    );
    agent.set_user_principals(Some(anonymous()));
    assert_eq!(agent.system_text().unwrap(), rendered);
}

#[test]
fn shared_and_session_variables_cannot_shadow_user_snapshot() {
    for mut agent in [developer_agent(), AgentConfig::from_prompt(USER_TEMPLATE)] {
        let expected = agent.system_text().unwrap();
        agent.set_shared_variables([("__user__".into(), "untrusted shared value".into())].into());
        assert_eq!(agent.system_text().unwrap(), expected);
        agent.set_session_variables([("__user__".into(), "untrusted session value".into())].into());
        assert_eq!(agent.system_text_with_tools(&[]).unwrap(), expected);
    }
}

#[test]
fn frontmatter_instructions_and_markdown_body_share_user_context() {
    for markdown in [
        format!("---\ninstructions: \"{USER_TEMPLATE}\"\n---\nUnused body"),
        USER_TEMPLATE.to_string(),
    ] {
        let mut agent = AgentConfig::from_markdown("user-template", &markdown).unwrap();
        // Loading a definition before session activation must also render safely.
        assert_eq!(agent.system_text().unwrap(), "None||||");
        agent.set_user_principals(Some(developer_agent().user_principals().unwrap().clone()));
        assert_eq!(
            agent.system_text().unwrap(),
            developer_agent().system_text().unwrap()
        );
    }
}
