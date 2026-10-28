use std::sync::Arc;

use effect_tsrs::{Diagnostic, Rule};
use serde_json::{Value, json};
use tsrs_api::{Handler, Response, Session, SessionOptions};
use tsrs_core::{
    ModuleKind, ModuleResolutionKind, ScriptTarget,
    context::{CheckerLifetime, Context, with_checker_lifetime},
};
use tsrs_vfs::{bundled, osvfs};

const IMPORT: &str = "import * as Effect from 'effect/Effect';\n";
const RULES: &[Rule] = &[Rule::FloatingEffect, Rule::MissingReturnYieldStar];

#[test]
fn effect_rules_use_the_callers_snapshot() {
    let session = Session::new(SessionOptions::new(
        "/".into(),
        bundled::lib_path(),
        Arc::new(bundled::wrap_fs(osvfs::fs())),
        false,
    ));
    let bad = format!("{IMPORT}const action = Effect.succeed(1);\nconst emoji = '😀'; action;");
    let (typescript, effect) = check(&session, &bad, Some(RULES), None);
    assert!(typescript.is_empty(), "{typescript:?}");
    assert_eq!(effect.len(), 1, "{effect:?}");
    assert_eq!(effect[0].name, "floatingEffect");
    assert_eq!(effect[0].code, 377001);
    assert_eq!(effect[0].location.file, "/index.ts");
    assert_eq!(effect[0].location.line, 3);
    assert_eq!(effect[0].location.column, 21);
    assert_eq!(effect[0].location.length, 6);
    assert_eq!(
        effect[0].location.start,
        bad[..bad.rfind("action;").unwrap()].encode_utf16().count()
    );
    // `check` has already released all snapshot ownership. Diagnostic text and
    // locations must remain usable, without retaining any TSRS pointer.
    println!(
        "owned diagnostic after snapshot release: {}",
        serde_json::to_string(&effect).unwrap()
    );

    let good = format!("{IMPORT}const action = Effect.succeed(1);\nEffect.runSync(action);");
    let (typescript, effect) = check(&session, &good, Some(RULES), None);
    assert!(
        typescript.is_empty() && effect.is_empty(),
        "{typescript:?} {effect:?}"
    );

    let bad_gen = format!(
        "{IMPORT}export const task = Effect.gen(function* () {{ yield* Effect.fail('oops'); }});"
    );
    let (typescript, effect) = check(&session, &bad_gen, Some(RULES), None);
    assert!(typescript.is_empty(), "{typescript:?}");
    assert_eq!(effect.len(), 1, "{effect:?}");
    assert_eq!(effect[0].name, "missingReturnYieldStar");
    let good_gen = bad_gen.replace("yield*", "return yield*");
    let (typescript, effect) = check(&session, &good_gen, Some(RULES), None);
    assert!(
        typescript.is_empty() && effect.is_empty(),
        "{typescript:?} {effect:?}"
    );

    let suppressed = format!(
        "{IMPORT}const action = Effect.succeed(1);\n// @effect-diagnostics-next-line floatingEffect:off\naction;"
    );
    let (typescript, effect) = check(&session, &suppressed, Some(RULES), None);
    assert!(
        typescript.is_empty() && effect.is_empty(),
        "{typescript:?} {effect:?}"
    );
    assert!(check(&session, &bad, Some(&[]), None).1.is_empty());

    for source in [
        "export const plain = () => 42; plain();",
        "const Effect = { succeed: () => 42 }; Effect.succeed();",
    ] {
        let (typescript, effect) = check(&session, source, Some(RULES), None);
        assert!(
            typescript.is_empty() && effect.is_empty(),
            "{typescript:?} {effect:?}"
        );
    }
    let (typescript, effect) = check(
        &session,
        "const value: number = 'wrong';",
        Some(RULES),
        None,
    );
    assert_eq!(typescript.len(), 1, "{typescript:?}");
    assert!(typescript[0].contains("not assignable"));
    assert!(effect.is_empty());

    let warning = r#"{"diagnosticSeverity":{"floatingEffect":"warning"}}"#;
    let (_, diagnostics) = check(&session, &bad, Some(RULES), Some(warning));
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].severity, effect_tsrs::Severity::Warning);
    let disabled = r#"{"diagnostics":false}"#;
    assert!(
        check(&session, &bad, Some(RULES), Some(disabled))
            .1
            .is_empty()
    );
    let overridden = r#"{"overrides":[{"include":["index.ts"],"options":{"diagnosticSeverity":{"floatingEffect":"off"}}}]}"#;
    assert!(
        check(&session, &bad, Some(RULES), Some(overridden))
            .1
            .is_empty()
    );

    // Same source/checker lifecycle with and without the new Effect call.
    // Print measurements without asserting an unproven production memory bound.
    for (label, rules) in [("baseline", None), ("effect", Some(RULES))] {
        let before = tsrs_core::ptr::reserve_stats();
        let arena_before = tsrs_core::ptr::arena_used_bytes();
        for index in 0..32 {
            let source = if index % 2 == 0 { &bad } else { &good };
            let (typescript, effect) = check(&session, source, rules, None);
            assert!(typescript.is_empty(), "{typescript:?}");
            assert_eq!(effect.len(), usize::from(rules.is_some() && index % 2 == 0));
        }
        println!(
            "{label} 32 cycles: reserved chunks before={before:?}, after={:?}; thread arena bytes before={arena_before}, after={}",
            tsrs_core::ptr::reserve_stats(),
            tsrs_core::ptr::arena_used_bytes(),
        );
    }
    session.close();
}

fn check(
    session: &Session,
    source: &str,
    rules: Option<&[Rule]>,
    lspconfig: Option<&str>,
) -> (Vec<String>, Vec<Diagnostic>) {
    let dependencies = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("node_modules")
        .canonicalize()
        .expect("run npm ci first");
    let created = call(
        session,
        "createSnapshot",
        json!({
            "createPrograms": [{
                "compilerOptions": {
                    "lib": ["lib.es2022.d.ts"],
                    "module": ModuleKind::PRESERVE.0,
                    "moduleResolution": ModuleResolutionKind::BUNDLER.0,
                    "noEmit": true,
                    "preserveSymlinks": true,
                    "skipLibCheck": true,
                    "strict": true,
                    "target": ScriptTarget::ES2022.0
                },
                "rootFiles": ["/index.ts"]
            }],
            "fileSystem": {
                "files": {"/index.ts": source, "/package.json": "{\"type\":\"module\"}"},
                "kind": "full",
                "symlinks": {
                    "/node_modules/effect": {"host": true, "target": dependencies.join("effect")},
                    "bundled:///libs": {"host": true, "target": "bundled:///libs"}
                }
            }
        }),
    );
    let snapshot = created["snapshot"].as_u64().unwrap();
    let project = tsrs_project::ID(
        created["operation"]["createdPrograms"][0]
            .as_str()
            .unwrap()
            .into(),
    );
    let data = session.snapshot_data(snapshot).unwrap();
    let weak = Arc::downgrade(&data);
    let program = data.get_program(&project).unwrap();
    let context = with_checker_lifetime(&Context::background(), CheckerLifetime::Diagnostics);
    let mut typescript = program.get_syntactic_diagnostics(&context, None);
    if typescript.is_empty() {
        typescript = program.get_semantic_diagnostics(&context, None);
    }
    let typescript: Vec<_> = typescript
        .iter()
        .map(|diagnostic| diagnostic.localize())
        .collect();
    let effect = if typescript.is_empty() {
        let mut setup = session.setup_checker(snapshot, &project).unwrap();
        assert!(std::ptr::eq(setup.program, program));
        let file = program.get_source_file("/index.ts").unwrap();
        // This is the proposed native integration point: the existing checker
        // and program go directly into upstream Effect rules in this process.
        rules.map_or_else(Vec::new, |rules| {
            let (cancelled, cancel) = context.with_cancel();
            cancel.call();
            assert_eq!(
                effect_tsrs::check_file(
                    &mut setup.checker,
                    &cancelled,
                    file,
                    lspconfig,
                    program,
                    Some(rules),
                )
                .unwrap_err(),
                "context canceled"
            );
            effect_tsrs::check_file(
                &mut setup.checker,
                &context,
                file,
                lspconfig,
                program,
                Some(rules),
            )
            .unwrap()
        })
    } else {
        Vec::new()
    };
    drop(data);
    call(session, "release", json!({"snapshot": snapshot}));
    assert!(session.snapshot_data(snapshot).is_err());
    assert!(weak.upgrade().is_none(), "snapshot still retained");
    (typescript, effect)
}

fn call(session: &Session, method: &str, params: Value) -> Value {
    match session
        .handle_request(method, &serde_json::to_vec(&params).unwrap())
        .unwrap()
    {
        Response::Json(text) => serde_json::from_str(&text).unwrap(),
        Response::Binary(_) => panic!("unexpected binary response for {method}"),
    }
}
