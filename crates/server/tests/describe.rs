use proto::catalog::{Catalog, Description};
use proto::*;
use serde_json::{Value, json};
use testkit::*;

#[tokio::test]
async fn describe_answers_the_catalog_with_the_server_s_release() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let catalog: Catalog = serde_json::from_value(c.ok("describe", json!({})).await).unwrap();
    assert_eq!(
        catalog,
        proto::catalog::catalog(Some(&server.registry.config.version))
    );
}

#[tokio::test]
async fn describe_with_a_method_answers_that_entry_alone() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let one: Description =
        serde_json::from_value(c.ok("describe", json!({"method": "wait"})).await).unwrap();
    assert_eq!(one.method.name, "wait");
    assert_eq!(one.method.params["$ref"], "#/$defs/WaitParams");
    assert!(one.defs.contains_key("Until"));
    assert!(
        !one.defs.contains_key("Session"),
        "only what wait refers to"
    );

    let err = c
        .request("describe", json!({"method": "no_such_method"}))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(err.message.contains("no_such_method"), "{}", err.message);
    assert_eq!(
        c.err("describe", json!({"method": 1})).await,
        ErrorCode::InvalidRequest
    );
}

#[tokio::test]
async fn the_catalog_lists_exactly_the_methods_the_server_dispatches() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let catalog: Catalog = serde_json::from_value(c.ok("describe", json!({})).await).unwrap();
    for method in &catalog.methods {
        let err = c.request(&method.name, Value::Null).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest, "{}", method.name);
        assert!(
            !err.message.starts_with("unknown method"),
            "{}: {}",
            method.name,
            err.message
        );
    }
    for name in ["no_such_method", "List", "api", ""] {
        let err = c.request(name, json!({})).await.unwrap_err();
        assert_eq!(err.message, format!("unknown method {name}"));
    }
}

#[tokio::test]
async fn a_live_reply_has_the_fields_its_schema_names() {
    let server = start(fast_limits());
    let mut c = client(&server).await;
    let catalog: Catalog = serde_json::from_value(c.ok("describe", json!({})).await).unwrap();
    let s = c.create(json!({"argv": ["/bin/cat"]})).await;
    let replies = [
        ("inspect", c.ok("inspect", json!({"session": s.id})).await),
        ("read", c.ok("read", json!({"session": s.id})).await),
        (
            "tail",
            c.ok("tail", json!({"session": s.id, "after": 0})).await,
        ),
        (
            "send",
            c.ok("send", json!({"session": s.id, "text": "x"})).await,
        ),
        ("list", c.ok("list", json!({})).await),
        ("ack", c.ok("ack", json!({"session": s.id})).await),
        ("cancel", c.ok("cancel", json!({"id": 99})).await),
        ("end", c.ok("end", json!({"session": s.id})).await),
    ];
    for (name, reply) in replies {
        let method = catalog.method(name).unwrap();
        let schema = &catalog.defs[method.result["$ref"]
            .as_str()
            .unwrap()
            .trim_start_matches("#/$defs/")];
        let properties = schema["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for field in schema["required"].as_array().into_iter().flatten() {
            assert!(
                reply.get(field.as_str().unwrap()).is_some(),
                "{name}: {field}"
            );
        }
        for field in reply.as_object().unwrap().keys() {
            assert!(properties.contains_key(field), "{name}: {field} undeclared");
        }
    }
}
