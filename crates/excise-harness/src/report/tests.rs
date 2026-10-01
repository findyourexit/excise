//! Report documents: the schemas compile, match the Rust constants and types, and reject drift.

use std::collections::{BTreeMap, BTreeSet};

use jsonschema::Validator;
use serde_json::Value;

use super::{
    AbContext, AbFixture, AbKind, AbVerdict, BinaryIdentity, BuildIdentity, ConfidenceInterval,
    Document, FailedStep, FailureKind, FixtureIdentity, HarnessAb, HarnessFailure, HarnessSummary,
    MetricComparison, Rusage, SCHEMA_VERSION, Samples, ScenarioResult, SchemaVersion,
    ScreenComparison, Side, SummaryKind, TerminalModes, Tier, Verdict,
};
use crate::scenario::{Expect, Profile};

const SHA256: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
const GIT_SHA: &str = "37d1c18a5f0c2b9e4d1a7c3b8e6f2a90d4c5b1e7";
const FIXTURE_HASH: &str = "3a7bd3e2360a3d29eea436fcfb7e44c735d117c42d1c1835420b6b9942dd4f1b";
const SCHEMA_ID_PREFIX: &str = "https://github.com/findyourexit/excise/harness/schemas/";

/// A summary with every optional field present.
fn summary() -> HarnessSummary {
    HarnessSummary {
        document_kind: SummaryKind::HarnessSummary,
        schema_version: SchemaVersion,
        run_id: "20260930T163000Z-3f9a".to_owned(),
        tier: Tier::Quick,
        started_at: "2026-09-30T16:30:00Z".to_owned(),
        finished_at: "2026-09-30T16:31:41.250+10:00".to_owned(),
        host: "ci-runner-7".to_owned(),
        os: "macos".to_owned(),
        arch: "aarch64".to_owned(),
        excise_binary: BinaryIdentity {
            path: "target/release/excise".to_owned(),
            sha256: SHA256.to_owned(),
        },
        git_sha: GIT_SHA.to_owned(),
        scenarios: vec![
            ScenarioResult {
                name: "delete-folder-lifecycle".to_owned(),
                profile: Profile::Default,
                verdict: Verdict::Pass,
                duration_ms: 4_180,
                metrics: BTreeMap::from([
                    ("max_rss_bytes".to_owned(), 20_971_520.0),
                    ("time_to_complete_ms".to_owned(), 3_720.5),
                ]),
                failure_bundle: None,
            },
            ScenarioResult {
                name: "signal-term-mid-scan".to_owned(),
                profile: Profile::Deterministic,
                verdict: Verdict::Xfail,
                duration_ms: 2_100,
                metrics: BTreeMap::new(),
                failure_bundle: Some(
                    "target/excise-e2e/20260930T163000Z-3f9a/signal-term-mid-scan-deterministic"
                        .to_owned(),
                ),
            },
        ],
    }
}

/// A summary with every optional field absent.
fn minimal_summary() -> HarnessSummary {
    HarnessSummary {
        scenarios: Vec::new(),
        ..summary()
    }
}

fn failure() -> HarnessFailure {
    HarnessFailure {
        document_kind: FailureKind::HarnessFailure,
        schema_version: SchemaVersion,
        scenario: "delete-folder-lifecycle".to_owned(),
        profile: Profile::MonochromeAscii,
        failed_step: FailedStep {
            index: 2,
            description: "delete victim (folder)".to_owned(),
        },
        screen: ScreenComparison {
            expected: "dialog names victim/".to_owned(),
            actual: "Delete keep-a.bin?\n[y] confirm".to_owned(),
        },
        terminal_modes: TerminalModes {
            alternate_screen: true,
            cursor_visible: false,
            echo: false,
            icanon: false,
        },
        cast_path: "target/excise-e2e/run/delete-folder-lifecycle.cast".to_owned(),
        rusage: Rusage {
            max_rss_bytes: 20_971_520,
            user_ms: 1_340,
            sys_ms: 210,
        },
        fixture: FixtureIdentity {
            hash: FIXTURE_HASH.to_owned(),
            seed: 18_446_744_073_709_551_615,
        },
        repro_command:
            "cargo xtask e2e --scenario delete-folder-lifecycle --profile monochrome-ascii"
                .to_owned(),
    }
}

fn ab() -> HarnessAb {
    HarnessAb {
        document_kind: AbKind::HarnessAb,
        schema_version: SchemaVersion,
        baseline: BuildIdentity {
            git_ref: "v1.3.0".to_owned(),
            binary_sha256: SHA256.to_owned(),
        },
        candidate: BuildIdentity {
            git_ref: "findyourexit/x1-drop-flushes".to_owned(),
            binary_sha256: SHA256.replace('9', "a"),
        },
        trials: 2,
        interleaving: vec![
            Side::Baseline,
            Side::Candidate,
            Side::Candidate,
            Side::Baseline,
        ],
        metrics: vec![MetricComparison {
            name: "delete-folder-lifecycle-default__time_to_complete_ms".to_owned(),
            samples: Samples {
                baseline: vec![3_900.0, 3_850.5],
                candidate: vec![1_100.0, 1_050.25],
            },
            median_ratio: 0.2789,
            bootstrap_ci: ConfidenceInterval {
                lower: 0.27,
                upper: 0.29,
                confidence: 0.95,
            },
            verdict: AbVerdict::Pass,
        }],
        context: AbContext {
            host: "macbook".to_owned(),
            cpu: "Apple M1 Pro".to_owned(),
            os: "macOS 26.0".to_owned(),
            arch: "aarch64".to_owned(),
            logical_cpus: 10,
            toolchain: "rustc 1.98.0".to_owned(),
            power: "ac".to_owned(),
            load_average_start: 1.2,
            load_average_end: 1.5,
            concurrent_excise_processes: 0,
            fixtures: vec![AbFixture {
                id: "delete-folder".to_owned(),
                hash: FIXTURE_HASH.to_owned(),
                seed: 7,
            }],
        },
    }
}

fn schema<D: Document>() -> Value {
    serde_json::from_str(D::SCHEMA_JSON).expect("the schema should be valid JSON")
}

/// A validator for `D`'s schema, optionally asserting `format` keywords.
fn validator<D: Document>(validate_formats: bool) -> Validator {
    jsonschema::draft202012::options()
        .should_validate_formats(validate_formats)
        .build(&schema::<D>())
        .expect("the schema should compile")
}

fn violations(validator: &Validator, instance: &Value) -> Vec<String> {
    validator
        .iter_errors(instance)
        .map(|error| error.to_string())
        .collect()
}

fn to_value<D: Document>(document: &D) -> Value {
    serde_json::to_value(document).expect("a document should serialize")
}

fn assert_schema_compiles<D: Document>() {
    let schema = schema::<D>();

    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema",
        "{} must declare draft 2020-12",
        D::KIND
    );
    jsonschema::draft202012::meta::validate(&schema)
        .unwrap_or_else(|error| panic!("{} is not a draft 2020-12 schema: {error}", D::KIND));
    validator::<D>(true);
}

fn assert_schema_identity<D: Document>() {
    let schema = schema::<D>();

    assert_eq!(schema["$id"], D::SCHEMA_ID);
    assert_eq!(
        D::SCHEMA_ID,
        format!("{SCHEMA_ID_PREFIX}{}-v{SCHEMA_VERSION}.json", D::KIND),
        "the $id follows <document_kind>-v<schema_version>"
    );
    assert_eq!(schema["properties"]["document_kind"]["const"], D::KIND);
    assert_eq!(
        schema["properties"]["schema_version"]["const"],
        SCHEMA_VERSION
    );
    for field in ["document_kind", "schema_version"] {
        assert!(
            schema["required"]
                .as_array()
                .expect("required is a list")
                .iter()
                .any(|name| name == field),
            "{} must require {field}",
            D::KIND
        );
    }
}

/// Every object schema that declares properties must reject undeclared ones.
fn assert_objects_are_closed(schema: &Value, at: &str) {
    if schema.get("properties").is_some() {
        assert_eq!(
            schema["additionalProperties"], false,
            "{at} declares properties and must set additionalProperties to false"
        );
    }
    let nested = schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .chain(
            schema
                .get("$defs")
                .and_then(Value::as_object)
                .into_iter()
                .flatten(),
        );
    for (name, child) in nested {
        assert_objects_are_closed(child, &format!("{at}/{name}"));
    }
}

/// Which declared properties an instance serialized, so a schema cannot drift from the types.
#[derive(Default)]
struct Coverage {
    declared: BTreeSet<String>,
    seen: BTreeSet<String>,
    definitions: BTreeSet<String>,
}

fn cover(root: &Value, schema: &Value, at: &str, instance: &Value, coverage: &mut Coverage) {
    let (schema, at) = match schema.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let target = reference
                .strip_prefix('#')
                .expect("only local references are used");
            if let Some(definition) = target.strip_prefix("/$defs/") {
                coverage.definitions.insert(definition.to_owned());
            }
            (
                root.pointer(target).expect("a reference should resolve"),
                target.to_owned(),
            )
        }
        None => (schema, at.to_owned()),
    };
    if let (Some(properties), Some(object)) = (
        schema.get("properties").and_then(Value::as_object),
        instance.as_object(),
    ) {
        for (name, property) in properties {
            coverage.declared.insert(format!("{at}/{name}"));
            if let Some(value) = object.get(name) {
                coverage.seen.insert(format!("{at}/{name}"));
                cover(
                    root,
                    property,
                    &format!("{at}/properties/{name}"),
                    value,
                    coverage,
                );
            }
        }
    }
    if let (Some(items), Some(array)) = (schema.get("items"), instance.as_array()) {
        for value in array {
            cover(root, items, &format!("{at}/items"), value, coverage);
        }
    }
    if let (Some(values), Some(object)) = (
        schema
            .get("additionalProperties")
            .filter(|value| value.is_object()),
        instance.as_object(),
    ) {
        for value in object.values() {
            cover(
                root,
                values,
                &format!("{at}/additionalProperties"),
                value,
                coverage,
            );
        }
    }
}

fn assert_schema_and_types_declare_the_same_fields<D: Document>(sample: &D) {
    let root = schema::<D>();
    let mut coverage = Coverage::default();

    cover(&root, &root, "", &to_value(sample), &mut coverage);

    let never_serialized: Vec<_> = coverage.declared.difference(&coverage.seen).collect();
    assert!(
        never_serialized.is_empty(),
        "{} declares fields the types never serialize: {never_serialized:?}",
        D::KIND
    );
    let defined: BTreeSet<String> = root["$defs"]
        .as_object()
        .expect("the schema has definitions")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        defined,
        coverage.definitions,
        "{} defines a shape that no serialized field uses",
        D::KIND
    );
}

fn assert_valid<D: Document>(document: &D) {
    let found = violations(&validator::<D>(true), &to_value(document));

    assert!(
        found.is_empty(),
        "a serialized {} must validate: {found:#?}",
        D::KIND
    );
}

fn assert_round_trips<D: Document + PartialEq + std::fmt::Debug>(document: &D) {
    let text = document.to_json_pretty().expect("a document should render");

    assert!(
        text.ends_with("}\n"),
        "the canonical form ends with one newline"
    );
    assert!(
        text.starts_with(&format!("{{\n  \"document_kind\": \"{}\",", D::KIND)),
        "the canonical form starts with the document kind:\n{text}"
    );
    assert_eq!(
        &D::from_json_str(&text).expect("the output should parse"),
        document
    );
    let rendered: Value = serde_json::from_str(&text).expect("the output is JSON");
    assert!(violations(&validator::<D>(true), &rendered).is_empty());
}

#[test]
fn every_schema_is_draft_2020_12_and_compiles() {
    assert_schema_compiles::<HarnessSummary>();
    assert_schema_compiles::<HarnessFailure>();
    assert_schema_compiles::<HarnessAb>();
}

#[test]
fn every_schema_identity_matches_the_rust_constants() {
    assert_schema_identity::<HarnessSummary>();
    assert_schema_identity::<HarnessFailure>();
    assert_schema_identity::<HarnessAb>();
    assert_eq!(SCHEMA_VERSION, 1);
}

#[test]
fn the_rust_marker_fields_serialize_the_document_constants() {
    for (document, kind) in [
        (to_value(&summary()), HarnessSummary::KIND),
        (to_value(&failure()), HarnessFailure::KIND),
        (to_value(&ab()), HarnessAb::KIND),
    ] {
        assert_eq!(document["document_kind"], kind);
        assert_eq!(document["schema_version"], SCHEMA_VERSION);
    }
}

#[test]
fn every_object_in_every_schema_rejects_undeclared_fields() {
    for schema in [
        schema::<HarnessSummary>(),
        schema::<HarnessFailure>(),
        schema::<HarnessAb>(),
    ] {
        assert_objects_are_closed(&schema, "#");
    }
}

#[test]
fn serialized_documents_validate_against_their_schemas() {
    assert_valid(&summary());
    assert_valid(&minimal_summary());
    assert_valid(&failure());
    assert_valid(&ab());
}

#[test]
fn schemas_and_types_declare_exactly_the_same_fields() {
    assert_schema_and_types_declare_the_same_fields(&summary());
    assert_schema_and_types_declare_the_same_fields(&failure());
    assert_schema_and_types_declare_the_same_fields(&ab());
}

#[test]
fn enumerations_match_between_schemas_and_types() {
    fn names<T: Copy>(all: &[T], name: impl Fn(T) -> &'static str) -> BTreeSet<String> {
        all.iter().map(|&value| name(value).to_owned()).collect()
    }
    fn declared(schema: &Value, pointer: &str) -> BTreeSet<String> {
        schema
            .pointer(pointer)
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{pointer} should be an enum"))
            .iter()
            .map(|value| value.as_str().expect("enum names are strings").to_owned())
            .collect()
    }
    let profiles = names(Profile::ALL, Profile::as_str);
    let summary_schema = schema::<HarnessSummary>();
    let ab_schema = schema::<HarnessAb>();

    assert_eq!(declared(&summary_schema, "/$defs/profile/enum"), profiles);
    assert_eq!(
        declared(&schema::<HarnessFailure>(), "/$defs/profile/enum"),
        profiles
    );
    assert_eq!(
        declared(&summary_schema, "/properties/tier/enum"),
        names(Tier::ALL, Tier::as_str)
    );
    assert_eq!(
        declared(&summary_schema, "/$defs/verdict/enum"),
        names(Verdict::ALL, Verdict::as_str)
    );
    assert_eq!(
        declared(&ab_schema, "/$defs/side/enum"),
        names(Side::ALL, Side::as_str)
    );
    assert_eq!(
        declared(&ab_schema, "/$defs/metric/properties/verdict/enum"),
        names(AbVerdict::ALL, AbVerdict::as_str)
    );
}

/// Sets the value at `pointer`, which must already exist.
fn set(document: &mut Value, pointer: &str, value: Value) {
    *document
        .pointer_mut(pointer)
        .unwrap_or_else(|| panic!("{pointer} should exist")) = value;
}

/// Removes the object member at `pointer`.
fn remove(document: &mut Value, pointer: &str) {
    let (parent, name) = pointer.rsplit_once('/').expect("a member pointer");
    document
        .pointer_mut(parent)
        .and_then(Value::as_object_mut)
        .unwrap_or_else(|| panic!("{parent} should be an object"))
        .remove(name);
}

/// One way to break a document, with a description for the failure message.
type Edit<'a> = (&'a str, &'a dyn Fn(&mut Value));

/// Asserts that each edit makes the document invalid.
fn assert_rejected<D: Document>(document: &D, edits: &[Edit<'_>]) {
    let validator = validator::<D>(true);
    for (description, edit) in edits {
        let mut instance = to_value(document);
        edit(&mut instance);

        assert!(
            !validator.is_valid(&instance),
            "{}: the schema must reject: {description}",
            D::KIND
        );
    }
}

#[test]
fn the_summary_schema_rejects_contract_drift() {
    assert_rejected(
        &summary(),
        &[
            ("an undeclared top-level field", &|d| {
                d["extra"] = true.into();
            }),
            ("an undeclared nested field", &|d| {
                d["scenarios"][0]["extra"] = 1.into();
            }),
            ("another document kind", &|d| {
                set(d, "/document_kind", "harness-ab".into());
            }),
            ("another schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("a missing git sha", &|d| remove(d, "/git_sha")),
            ("a missing scenario verdict", &|d| {
                remove(d, "/scenarios/0/verdict");
            }),
            ("an unknown tier", &|d| set(d, "/tier", "hourly".into())),
            ("an unknown verdict", &|d| {
                set(d, "/scenarios/0/verdict", "skip".into());
            }),
            ("an unknown profile", &|d| {
                set(d, "/scenarios/0/profile", "fancy".into());
            }),
            ("a run id that could leave its directory", &|d| {
                set(d, "/run_id", "../escape".into());
            }),
            ("an uppercase digest", &|d| {
                set(d, "/excise_binary/sha256", SHA256.to_uppercase().into());
            }),
            ("a short git sha", &|d| set(d, "/git_sha", "37d1c18".into())),
            ("a timestamp without a zone", &|d| {
                set(d, "/started_at", "2026-09-30T16:30:00".into());
            }),
            ("a timestamp with a space", &|d| {
                set(d, "/started_at", "2026-09-30 16:30:00Z".into());
            }),
            ("a negative duration", &|d| {
                set(d, "/scenarios/0/duration_ms", (-1).into());
            }),
            ("a metric that is not a number", &|d| {
                set(d, "/scenarios/0/metrics/max_rss_bytes", "big".into());
            }),
            ("a metric name that is not an identifier", &|d| {
                set(
                    d,
                    "/scenarios/0/metrics",
                    serde_json::json!({ "Max RSS": 1.0 }),
                );
            }),
            ("a scenario name that is not an identifier", &|d| {
                set(d, "/scenarios/0/name", "Delete Folder".into());
            }),
            ("an empty failure bundle path", &|d| {
                set(d, "/scenarios/1/failure_bundle", "".into());
            }),
        ],
    );
}

#[test]
fn the_failure_schema_rejects_contract_drift() {
    assert_rejected(
        &failure(),
        &[
            ("an undeclared field", &|d| d["extra"] = true.into()),
            ("an undeclared nested field", &|d| {
                d["terminal_modes"]["extra"] = true.into();
            }),
            ("another document kind", &|d| {
                set(d, "/document_kind", "harness-summary".into());
            }),
            ("another schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("a missing terminal mode", &|d| {
                remove(d, "/terminal_modes/icanon");
            }),
            ("a terminal mode that is not a boolean", &|d| {
                set(d, "/terminal_modes/echo", "yes".into());
            }),
            ("a negative step index", &|d| {
                set(d, "/failed_step/index", (-1).into());
            }),
            ("a step index beyond 32 bits", &|d| {
                set(d, "/failed_step/index", 4_294_967_296_u64.into());
            }),
            ("an empty step description", &|d| {
                set(d, "/failed_step/description", "".into());
            }),
            ("a missing screen text", &|d| remove(d, "/screen/actual")),
            ("a hash that is not hexadecimal", &|d| {
                set(d, "/fixture/hash", "XYZ".into());
            }),
            ("a negative seed", &|d| set(d, "/fixture/seed", (-1).into())),
            ("a negative resident set", &|d| {
                set(d, "/rusage/max_rss_bytes", (-1).into());
            }),
            ("an empty repro command", &|d| {
                set(d, "/repro_command", "".into());
            }),
            ("an unknown profile", &|d| {
                set(d, "/profile", "fancy".into());
            }),
        ],
    );
}

#[test]
fn the_ab_schema_rejects_contract_drift() {
    assert_rejected(
        &ab(),
        &[
            ("an undeclared field", &|d| d["extra"] = true.into()),
            ("an undeclared nested field", &|d| {
                d["metrics"][0]["bootstrap_ci"]["extra"] = true.into();
            }),
            ("another document kind", &|d| {
                set(d, "/document_kind", "harness-failure".into());
            }),
            ("another schema version", &|d| {
                set(d, "/schema_version", 2.into());
            }),
            ("a run order of one", &|d| {
                set(d, "/interleaving", serde_json::json!(["baseline"]));
            }),
            ("an unknown side in the run order", &|d| {
                set(
                    d,
                    "/interleaving",
                    serde_json::json!(["baseline", "control"]),
                );
            }),
            ("a certain confidence interval", &|d| {
                set(d, "/metrics/0/bootstrap_ci/confidence", 1.0.into());
            }),
            ("a zero confidence interval", &|d| {
                set(d, "/metrics/0/bootstrap_ci/confidence", 0.into());
            }),
            ("an unknown verdict", &|d| {
                set(d, "/metrics/0/verdict", "fail".into());
            }),
            ("a negative trial count", &|d| {
                set(d, "/trials", (-1).into());
            }),
            ("no baseline samples", &|d| {
                set(d, "/metrics/0/samples/baseline", serde_json::json!([]));
            }),
            ("a fractional process count", &|d| {
                set(d, "/context/concurrent_excise_processes", 1.5.into());
            }),
            ("a digest of the wrong length", &|d| {
                set(d, "/candidate/binary_sha256", "abc".into());
            }),
            ("a negative median ratio", &|d| {
                set(d, "/metrics/0/median_ratio", (-0.5).into());
            }),
            ("zero logical cpus", &|d| {
                set(d, "/context/logical_cpus", 0.into());
            }),
            ("a negative load average", &|d| {
                set(d, "/context/load_average_start", (-0.1).into());
            }),
            ("an empty fixtures list", &|d| {
                set(d, "/context/fixtures", serde_json::json!([]));
            }),
            ("a fixture hash that is not hexadecimal", &|d| {
                set(d, "/context/fixtures/0/hash", "not-hex".into());
            }),
            ("a missing fixtures field", &|d| {
                remove(d, "/context/fixtures");
            }),
            ("a missing context field", &|d| remove(d, "/context/power")),
        ],
    );
}

#[test]
fn timestamps_are_checked_for_shape_and_for_valid_dates() {
    let strict = validator::<HarnessSummary>(true);
    let lenient = validator::<HarnessSummary>(false);
    let with_start = |text: &str| {
        let mut instance = to_value(&summary());
        set(&mut instance, "/started_at", text.into());
        instance
    };

    for accepted in [
        "2026-09-30T16:30:00Z",
        "2026-09-30T16:30:00.123456Z",
        "2026-09-30T16:30:00+10:00",
        "2026-09-30T06:30:00-10:00",
    ] {
        assert!(strict.is_valid(&with_start(accepted)), "{accepted}");
    }
    for malformed in [
        "yesterday",
        "2026-09-30",
        "2026-09-30T16:30:00",
        "2026-09-30 16:30:00Z",
    ] {
        assert!(
            !lenient.is_valid(&with_start(malformed)),
            "{malformed} fails the pattern even when formats are not asserted"
        );
    }
    assert!(
        !strict.is_valid(&with_start("2026-13-45T25:61:61Z")),
        "an impossible date is caught by the date-time format"
    );
}

#[test]
fn documents_render_a_canonical_form_that_round_trips() {
    assert_round_trips(&summary());
    assert_round_trips(&minimal_summary());
    assert_round_trips(&failure());
    assert_round_trips(&ab());
}

#[test]
fn documents_that_are_not_this_kind_and_version_are_rejected() {
    let text = summary().to_json_pretty().expect("a summary should render");

    assert!(HarnessFailure::from_json_str(&text).is_err());
    assert!(HarnessAb::from_json_str(&text).is_err());
    assert!(HarnessSummary::from_json_str(&text.replace("harness-summary", "harness-ab")).is_err());
    let future = text.replace("\"schema_version\": 1", "\"schema_version\": 2");
    let error = HarnessSummary::from_json_str(&future).expect_err("version 2 is unsupported");
    assert!(
        error.to_string().contains("unsupported schema_version 2"),
        "{error}"
    );
    for malformed in [
        text.replace("\"schema_version\": 1", "\"schema_version\": 1.0"),
        text.replace("\"schema_version\": 1", "\"schema_version\": \"1\""),
        text.replace("\"host\"", "\"unexpected\": 1,\n  \"host\""),
        text.replace("\"tier\": \"quick\"", "\"tier\": \"hourly\""),
    ] {
        assert!(
            HarnessSummary::from_json_str(&malformed).is_err(),
            "{malformed}"
        );
    }
}

#[test]
fn a_document_that_json_cannot_carry_is_refused_rather_than_written_as_null() {
    let mut not_a_number = summary();
    not_a_number.scenarios[0]
        .metrics
        .insert("ratio".to_owned(), f64::NAN);
    let mut infinite = ab();
    infinite.metrics[0].samples.candidate[0] = f64::INFINITY;

    assert!(not_a_number.to_json_pretty().is_err());
    assert!(infinite.to_json_pretty().is_err());
}

#[test]
fn strict_xfail_is_encoded_in_verdict_resolution() {
    assert_eq!(Verdict::resolve(Expect::Pass, true), Verdict::Pass);
    assert_eq!(Verdict::resolve(Expect::Pass, false), Verdict::Fail);
    assert_eq!(Verdict::resolve(Expect::Fail, false), Verdict::Xfail);
    assert_eq!(
        Verdict::resolve(Expect::Fail, true),
        Verdict::Xpass,
        "an expected failure that passes is never a pass"
    );
}

#[test]
fn only_pass_and_xfail_keep_a_run_green() {
    let green: Vec<Verdict> = Verdict::ALL
        .iter()
        .copied()
        .filter(|verdict| !verdict.blocks_run())
        .collect();

    assert_eq!(green, [Verdict::Pass, Verdict::Xfail]);
}
