use super::*;
use wasmtime::{Config, Engine};

// One concrete diagnostic ABI fixture. All behavior below executes real Wasm.
const COMPONENT: &str = r#"(component
    @IMPORT@
    (type $request (record
        (field "world-instance" u64) (field "dimension" string) (field "type-id" string)
        (field "schema" u32) (field "x" s32) (field "y" s32) (field "z" s32)
        (field "instance" u64) (field "counter" u64) (field "game-time" s64)))
    (type $proposal (record (field "world-instance" u64) (field "instance" u64)
        (field "game-time" s64) (field "expected-counter" u64) (field "next-counter" u64)))
    (type $rejection (enum "rejected"))
    (type $step (func (param "input" $request) (result (result $proposal (error $rejection)))))
    (core module $m
        (memory (export "memory") 1)
        (data (i32.const 4096) "minecraft:overworld") (data (i32.const 4200) "test:counter")
        (global $heap (mut i32) (i32.const 8192))
        (global $calls (mut i32) (i32.const 0))
        @MODULE@
        (func (export "realloc") (param i32 i32 i32 i32) (result i32) (local $old i32)
            @REALLOC@)
        (func $check-string (param $p i32) (param $len i32) (param $expected i32) (param $n i32)
            (local $i i32)
            (if (i32.ne (local.get $len) (local.get $n)) (then unreachable))
            (loop $bytes
                (if (i32.ne (i32.load8_u (i32.add (local.get $p) (local.get $i)))
                    (i32.load8_u (i32.add (local.get $expected) (local.get $i)))) (then unreachable))
                (local.set $i (i32.add (local.get $i) (i32.const 1)))
                (br_if $bytes (i32.lt_u (local.get $i) (local.get $n)))))
        (func $burn (param $n i32)
            (loop $spin (local.set $n (i32.sub (local.get $n) (i32.const 1)))
                (br_if $spin (local.get $n))))
        (func (export "step")
            (param $world i64) (param $dimension i32) (param $dimension-len i32)
            (param $type i32) (param $type-len i32) (param $schema i32)
            (param $x i32) (param $y i32) (param $z i32)
            (param $instance i64) (param $counter i64) (param $time i64) (result i32)
            @BODY@)
        (func (export "post") (param i32) @POST@))
    (core instance $i (instantiate $m))
    @INSTANCES@
    (func $step-func (type $step) (canon lift (core func $i "step")
        (memory $i "memory") (realloc (func $i "realloc")) (post-return (func $i "post"))))
    (instance $api (export "request" (type $request)) (export "proposal" (type $proposal))
        (export "rejection" (type $rejection)) (export "step" (func $step-func)))
    (export "counter-trial" (instance $api)))"#;
const REALLOC: &str =
    "global.get $heap local.tee $old local.get 3 i32.add global.set $heap local.get $old";
const REPLY: &str = r#"
    (i32.store (i32.const 0) (i32.const 0))
    (i64.store (i32.const 8) (local.get $world))
    (i64.store (i32.const 16) (local.get $instance))
    (i64.store (i32.const 24) (local.get $time))
    (i64.store (i32.const 32) (local.get $counter))
    (i64.store (i32.const 40) (i64.add (local.get $counter) (i64.const 1)))
    i32.const 0"#;
const CHECK_REQUEST: &str = r#"
    (if (i64.ne (local.get $world) (i64.const 17)) (then unreachable))
    (call $check-string (local.get $dimension) (local.get $dimension-len) (i32.const 4096) (i32.const 19))
    (call $check-string (local.get $type) (local.get $type-len) (i32.const 4200) (i32.const 12))
    (if (i32.ne (local.get $schema) (i32.const 1)) (then unreachable))
    (if (i32.ne (local.get $x) (i32.const -5)) (then unreachable))
    (if (i32.ne (local.get $y) (i32.const 80)) (then unreachable))
    (if (i32.ne (local.get $z) (i32.const 12)) (then unreachable))
    (if (i64.ne (local.get $instance) (i64.const 23)) (then unreachable))
    (if (i64.ne (local.get $counter) (i64.const 9)) (then unreachable))
    (if (i64.ne (local.get $time) (i64.const -22)) (then unreachable))
    (global.set $calls (i32.add (global.get $calls) (i32.const 1)))
    (if (i32.ne (global.get $calls) (i32.const 1)) (then unreachable))"#;

fn engine() -> Engine {
    let mut config = Config::new();
    config.wasm_component_model(true).consume_fuel(true);
    Engine::new(&config).unwrap()
}
fn source(body: &str) -> String {
    COMPONENT
        .replace("@IMPORT@", "")
        .replace("@MODULE@", "")
        .replace("@INSTANCES@", "")
        .replace("@REALLOC@", REALLOC)
        .replace("@BODY@", body)
        .replace("@POST@", "")
}
fn compile(engine: &Engine, source: &str) -> Component {
    Component::new(
        engine,
        wat::parse_str(source).expect("valid authored component WAT"),
    )
    .expect("valid authored component ABI")
}
fn evaluator(engine: &Engine, source: &str) -> CounterEvaluator {
    CounterEvaluator::checked(&compile(engine, source), [7; 32]).unwrap()
}
fn request() -> CounterRequest {
    CounterRequest {
        world_instance: 17,
        dimension: "minecraft:overworld".into(),
        type_id: "test:counter".into(),
        schema: 1,
        x: -5,
        y: 80,
        z: 12,
        instance: 23,
        counter: 9,
        game_time: -22,
    }
}

#[test]
fn actual_request_projection_plus_one_and_fresh_store_discard_guest_globals() {
    let engine = engine();
    let input = request();
    let before = input.clone();
    let component = compile(&engine, &source(&format!("{CHECK_REQUEST}{REPLY}")));
    assert_eq!(component.component_type().imports(&engine).count(), 0);
    let program = CounterEvaluator::checked(&component, [7; 32]).unwrap();
    assert_eq!(program.identity(), &[7; 32]);
    let expected = CounterProposal {
        world_instance: 17,
        instance: 23,
        game_time: -22,
        expected_counter: 9,
        next_counter: 10,
    };
    assert_eq!(program.evaluate(&input).unwrap(), expected);
    assert_eq!(program.evaluate(&input).unwrap(), expected);
    assert_eq!(input, before);
    let program = evaluator(&engine, &source(REPLY));
    let mut last = input;
    last.counter = u64::MAX - 1;
    assert_eq!(program.evaluate(&last).unwrap().next_counter, u64::MAX);
    let fail_after_global_write = format!(
        r#"
        (global.set $calls (i32.add (global.get $calls) (i32.const 1)))
        (if (i32.ne (global.get $calls) (i32.const 1)) (then unreachable))
        (if (i64.eq (local.get $counter) (i64.const 9)) (then unreachable)) {REPLY}"#
    );
    let program = evaluator(&engine, &source(&fail_after_global_write));
    assert!(program.evaluate(&request()).is_err());
    let mut after_failure = request();
    after_failure.counter = 10;
    assert_eq!(program.evaluate(&after_failure).unwrap().next_counter, 11);
}

#[test]
fn imports_missing_and_wrong_exports_refuse_before_trapping_start() {
    let engine = engine();
    let trapping = COMPONENT.replace("@MODULE@", "(func $start unreachable) (start $start)");
    for name in [
        "horizon:mods/world@0.1.0",
        "horizon:mods/storage@0.1.0",
        "horizon:mods/log@0.1.0",
        "horizon:mods/players@0.1.0",
        "wasi:filesystem/types@0.2.0",
    ] {
        let imported = trapping.replace(
            "@IMPORT@",
            &format!("(type $imported (instance)) (import \"{name}\" (instance (type $imported)))"),
        );
        let imported = imported
            .replace("@BODY@", REPLY)
            .replace("@REALLOC@", REALLOC)
            .replace("@POST@", "")
            .replace("@INSTANCES@", "");
        let error = CounterEvaluator::checked(&compile(&engine, &imported), [0; 32])
            .err()
            .unwrap();
        assert_eq!(error, "counter-trial requires zero component imports");
    }
    let trapping = trapping
        .replace("@IMPORT@", "")
        .replace("@BODY@", REPLY)
        .replace("@REALLOC@", REALLOC)
        .replace("@POST@", "")
        .replace("@INSTANCES@", "");
    for (from, to, expected) in [
        (
            "counter-trial",
            "client-only-counter",
            "missing counter-trial interface",
        ),
        (
            "(export \"step\" (func $step-func))",
            "(export \"other\" (func $step-func))",
            "missing counter-trial.step function",
        ),
    ] {
        let component = compile(&engine, &trapping.replace(from, to));
        assert_eq!(
            CounterEvaluator::checked(&component, [0; 32])
                .err()
                .unwrap(),
            expected
        );
    }
    let wrong = trapping.replace("(field \"schema\" u32)", "(field \"schema\" s32)");
    let error = CounterEvaluator::checked(&compile(&engine, &wrong), [0; 32])
        .err()
        .unwrap();
    assert!(error.starts_with("counter-trial step type:"));
    assert!(error.chars().count() <= 512);
}

#[test]
fn async_abi_is_refused_by_the_current_engine_before_start() {
    let engine = engine();
    let text = source(REPLY)
        .replace(
            "(global $calls",
            "(func $start unreachable) (start $start) (global $calls",
        )
        .replace("(type $step (func", "(type $step (func async");
    let bytes = wat::parse_str(text).unwrap();
    let Err(error) = Component::new(&engine, bytes) else {
        panic!("async ABI is disabled in the runtime engine");
    };
    assert!(format!("{error:#}").contains("async"));
}

#[test]
fn actual_start_body_realloc_post_return_and_lifting_failures_yield_no_proposal() {
    let engine = engine();
    let base = source(REPLY);
    let cases = [
        base.replace(
            "(global $calls",
            "(func $start unreachable) (start $start) (global $calls",
        ),
        base.replace(
            "(global $calls",
            "(func $start (loop $forever (br $forever))) (start $start) (global $calls",
        ),
        source("unreachable"),
        source("(loop $forever (br $forever)) unreachable"),
        base.replace(REALLOC, "unreachable"),
        base.replace(REALLOC, "(loop $forever (br $forever)) unreachable"),
        base.replace(REALLOC, "i32.const 65536"),
        base.replace(
            "(func (export \"post\") (param i32) )",
            "(func (export \"post\") (param i32) unreachable)",
        ),
        base.replace(
            "(func (export \"post\") (param i32) )",
            "(func (export \"post\") (param i32) (loop $forever (br $forever)))",
        ),
        source("(i32.store (i32.const 0) (i32.const 2)) i32.const 0"),
        source(
            "(i32.store (i32.const 0) (i32.const 1)) (i32.store (i32.const 8) (i32.const 1)) i32.const 0",
        ),
        source("i32.const 1"),
        source("i32.const 65536"),
    ];
    for (case, text) in cases.iter().enumerate() {
        let program = evaluator(&engine, text);
        let input = request();
        let before = input.clone();
        assert!(
            program.evaluate_with(&input, 2_000, limits()).is_err(),
            "case {case}"
        );
        assert_eq!(input, before);
    }
}

#[test]
fn instantiation_and_step_share_one_unreplenished_fuel_budget() {
    let engine = engine();
    let base = source(REPLY);
    let start = "(func $start (call $burn (i32.const 50))) (start $start)";
    let only_start = base.replace("(global $calls", &format!("{start} (global $calls"));
    let only_body = source(&format!("(call $burn (i32.const 50)) {REPLY}"));
    let both = only_body.replace("(global $calls", &format!("{start} (global $calls"));
    for text in [&only_start, &only_body] {
        evaluator(&engine, text)
            .evaluate_with(&request(), 400, limits())
            .unwrap();
    }
    let error = evaluator(&engine, &both)
        .evaluate_with(&request(), 400, limits())
        .unwrap_err();
    assert!(error.contains("fuel"));
}

#[test]
fn actual_memory_table_and_instance_limits_refuse_excess_and_allow_boundaries() {
    let engine = engine();
    let base = source(REPLY);
    let too_many_instances = format!(
        "(core module $empty) {}",
        "(core instance (instantiate $empty))".repeat(16)
    );
    let allowed_instances = format!(
        "(core module $empty) {}",
        "(core instance (instantiate $empty))".repeat(15)
    );
    let with_instances =
        |extra: &str| base.replacen("(func $step-func", &format!("{extra} (func $step-func"), 1);
    let cases = [
        base.replace(
            "(memory (export \"memory\") 1)",
            "(memory (export \"memory\") 1025)",
        ),
        base.replace("(global $calls", "(memory 0) (global $calls"),
        base.replace(
            "(global $calls",
            "(table 1 funcref) (table 1 funcref) (table 1 funcref) (global $calls",
        ),
        base.replace("(global $calls", "(table 16385 funcref) (global $calls"),
        source(&format!("(drop (memory.grow (i32.const 1024))) {REPLY}")),
        source(&format!(
            "(drop (table.grow $t (ref.null func) (i32.const 16384))) {REPLY}"
        ))
        .replace("(global $calls", "(table $t 1 funcref) (global $calls"),
        with_instances(&too_many_instances),
    ];
    for (case, text) in cases.iter().enumerate() {
        assert!(
            evaluator(&engine, text).evaluate(&request()).is_err(),
            "limit case {case}"
        );
    }
    evaluator(&engine, &with_instances(&allowed_instances))
        .evaluate(&request())
        .unwrap();
    let boundary = base.replace(
        "(global $calls",
        "(table 16384 funcref) (table 16384 funcref) (global $calls",
    );
    evaluator(&engine, &boundary).evaluate(&request()).unwrap();
    let memory_boundary = base.replace(
        "(memory (export \"memory\") 1)",
        "(memory (export \"memory\") 1024)",
    );
    evaluator(&engine, &memory_boundary)
        .evaluate(&request())
        .unwrap();
}

#[test]
fn guest_rejection_and_every_invalid_scalar_are_refused_without_mutating_request() {
    let engine = engine();
    let input = request();
    let before = input.clone();
    for (from, to) in [
        (
            "(i32.const 8) (local.get $world)",
            "(i32.const 8) (i64.const 18)",
        ),
        (
            "(i32.const 16) (local.get $instance)",
            "(i32.const 16) (i64.const 24)",
        ),
        (
            "(i32.const 24) (local.get $time)",
            "(i32.const 24) (i64.const -21)",
        ),
        (
            "(i32.const 32) (local.get $counter)",
            "(i32.const 32) (i64.const 8)",
        ),
        (
            "(i64.add (local.get $counter) (i64.const 1))",
            "(local.get $counter)",
        ),
        ("(i64.const 1)", "(i64.const 2)"),
        (
            "(i64.add (local.get $counter) (i64.const 1))",
            "(i64.const 0)",
        ),
    ] {
        let program = evaluator(&engine, &source(&REPLY.replace(from, to)));
        assert_eq!(
            program.evaluate(&input).unwrap_err(),
            "counter-trial invalid scalar proposal"
        );
        assert_eq!(input, before);
    }
    let rejected = source("(i32.store (i32.const 0) (i32.const 1)) i32.const 0");
    assert_eq!(
        evaluator(&engine, &rejected).evaluate(&input).unwrap_err(),
        "counter-trial guest rejected"
    );
    let trapping = source(REPLY).replace(
        "(global $calls",
        "(func $start unreachable) (start $start) (global $calls",
    );
    let mut exhausted = input;
    exhausted.counter = u64::MAX;
    assert_eq!(
        evaluator(&engine, &trapping)
            .evaluate(&exhausted)
            .unwrap_err(),
        "counter-trial counter exhausted"
    );
}

#[test]
fn request_byte_caps_refuse_before_start_and_bound_returned_diagnostics() {
    let engine = engine();
    let program = evaluator(&engine, &source(REPLY));
    let mut input = request();
    input.dimension = "x".repeat(256);
    input.type_id = "é".repeat(128);
    program.evaluate(&input).unwrap();
    let trapping = source(REPLY).replace(
        "(global $calls",
        "(func $start unreachable) (start $start) (global $calls",
    );
    let program = evaluator(&engine, &trapping);
    for too_long in [0, 1] {
        let mut input = request();
        if too_long == 0 {
            input.dimension = "x".repeat(257);
        } else {
            input.type_id = "é".repeat(129);
        }
        assert_eq!(
            program.evaluate(&input).unwrap_err(),
            "counter-trial request identifier exceeds 256 bytes"
        );
    }
    let message = diagnostic("test", wasmtime::Error::msg("é".repeat(600)));
    assert_eq!(message.chars().count(), 512);
}
