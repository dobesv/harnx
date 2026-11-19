use super::*;

#[test]
fn interaction_surface_rejects_self_reference() {
    rejects(
        |form| form["createSurface"]["components"][0]["child"] = json!("root"),
        &["component reference cycle"],
    );
    rejects(
        |form| form["createSurface"]["components"][6]["child"] = json!("submit"),
        &["component reference cycle"],
    );
}

#[test]
fn interaction_surface_rejects_two_node_cycle() {
    rejects(
        |form| form["createSurface"]["components"][1]["children"][0] = json!("root"),
        &["component reference cycle"],
    );
}

#[test]
fn interaction_surface_rejects_template_cycle() {
    rejects(
        |form| {
            form["createSurface"]["components"][1]["children"] =
                json!({"componentId": "col", "path": "/items"});
        },
        &["component reference cycle"],
    );
}

#[test]
fn interaction_surface_rejects_unreachable_cycle() {
    rejects(
        |form| {
            form["createSurface"]["components"]
                .as_array_mut()
                .unwrap()
                .push(json!({"id": "detached", "component": "Card", "child": "detached"}));
        },
        &["component reference cycle"],
    );
}

fn graph_form(mut components: Vec<Value>) -> Value {
    components.extend([
        json!({"id": "leaf", "component": "Text", "text": "Shared"}),
        json!({"id": "go", "component": "Button", "child": "leaf",
               "action": {"event": {"name": "go"}}}),
    ]);
    json!({"version": "1.0", "createSurface": {"components": components}})
}

fn deep_form(depth: usize) -> Value {
    let mut nodes = vec![json!({"id": "root", "component": "Card", "child": "n1"})];
    for index in 1..depth - 1 {
        let child = if index == depth - 2 {
            "leaf".into()
        } else {
            format!("n{}", index + 1)
        };
        nodes.push(json!({"id": format!("n{index}"), "component": "Card", "child": child}));
    }
    graph_form(nodes)
}

#[test]
fn interaction_surface_enforces_depth_bound_in_either_list_order() {
    for reverse in [false, true] {
        for (depth, valid) in [
            (MAX_COMPONENT_DEPTH, true),
            (MAX_COMPONENT_DEPTH + 1, false),
        ] {
            let mut form = deep_form(depth);
            if reverse {
                form["createSurface"]["components"]
                    .as_array_mut()
                    .unwrap()
                    .reverse();
            }
            let result = validate_surface(&form);
            if valid {
                result.unwrap();
            } else {
                assert!(result.unwrap_err().contains("depth limit"));
            }
        }
    }
}

#[test]
fn interaction_surface_accepts_shared_dag() {
    let form = graph_form(vec![
        json!({"id": "root", "component": "Column", "children": ["a", "b", "go"]}),
        json!({"id": "a", "component": "Card", "child": "leaf"}),
        json!({"id": "b", "component": "Card", "child": "leaf"}),
    ]);
    validate_surface(&form).unwrap();
}

#[test]
fn interaction_surface_enforces_expansion_bound() {
    for (visits, valid) in [
        (MAX_COMPONENT_EXPANSION, true),
        (MAX_COMPONENT_EXPANSION + 1, false),
    ] {
        let form = graph_form(vec![json!({
            "id": "root", "component": "Column", "children": vec!["leaf"; visits - 1]
        })]);
        let result = validate_surface(&form);
        if valid {
            result.unwrap();
        } else {
            assert!(result.unwrap_err().contains("expansion limit"));
        }
    }
}

#[test]
fn interaction_surface_rejects_exponential_shared_dag() {
    let mut nodes = vec![json!({"id": "root", "component": "Column", "children": ["n1", "n1"]})];
    for index in 1..12 {
        let child = if index == 11 {
            "leaf".into()
        } else {
            format!("n{}", index + 1)
        };
        nodes.push(
            json!({"id": format!("n{index}"), "component": "Column", "children": [&child, &child]}),
        );
    }
    let error = validate_surface(&graph_form(nodes)).unwrap_err();
    assert!(error.contains("expansion limit"), "{error}");
}

#[test]
fn interaction_surface_rejects_oversized_component_list() {
    let mut form = valid_form();
    form["createSurface"]["components"] = json!(vec![Value::Null; MAX_COMPONENT_EXPANSION + 1]);
    let error = validate_surface(&form).unwrap_err();
    assert!(error.contains("component limit"), "{error}");
}
