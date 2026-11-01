//! Security triggers decide which changes get a security review. A match selects review, and no match is not
//! proof of safety. Ported from `teams-evidence.test.ts`, with Rust process, network, filesystem, and
//! permission cases added.

use kratos_teams::evidence::{Diff, DiffFile};
use kratos_teams::security_triggers::{
    DEFAULT_PATTERNS, LineProblem, MAX_HIT_LINE, MAX_HITS, Trigger, TriggerError, TriggerHit,
    default_triggers, match_triggers, parse_triggers,
};

fn file(path: &str, added: &[&str]) -> DiffFile {
    DiffFile {
        path: path.to_owned(),
        ranges: vec![],
        added: added.iter().map(|line| (*line).to_owned()).collect(),
    }
}

fn path_trigger(glob: &str) -> Trigger {
    Trigger::Path {
        glob: glob.to_owned(),
    }
}

fn code_trigger(pattern: &str) -> Trigger {
    Trigger::Code {
        pattern: pattern.to_owned(),
    }
}

#[test]
fn the_kratos_security_block_of_review_md_holds_the_triggers_and_other_blocks_are_ignored() {
    let review_md = [
        "# Review",
        "```kratos-security",
        "# Providers start processes.",
        "path: packages/engine/src/providers/**",
        "",
        r"code: child_process|spawn\(",
        "```",
        "```ts",
        "path: not/a/trigger",
        "```",
    ]
    .join("\n");

    assert_eq!(
        parse_triggers(Some(&review_md)),
        Ok(vec![
            path_trigger("packages/engine/src/providers/**"),
            code_trigger(r"child_process|spawn\("),
        ])
    );
}

#[test]
fn without_a_block_the_defaults_apply_and_an_empty_block_turns_the_pass_off() {
    assert_eq!(parse_triggers(None), Ok(default_triggers()));
    assert_eq!(
        parse_triggers(Some("# Review\nNo triggers here.")),
        Ok(default_triggers())
    );
    assert_eq!(
        parse_triggers(Some("~~~kratos-security\n# Off for this project.\n~~~")),
        Ok(vec![])
    );
}

#[test]
fn a_fence_may_be_indented_up_to_three_spaces_and_an_unclosed_block_runs_to_the_end() {
    let indented = "   ```kratos-security\npath: src/**\n   ```\n";
    let unclosed = "```kratos-security\npath: src/**\ncode: spawn";
    let too_deep = "    ```kratos-security\npath: src/**\n```";

    assert_eq!(
        parse_triggers(Some(indented)),
        Ok(vec![path_trigger("src/**")])
    );
    assert_eq!(
        parse_triggers(Some(unclosed)),
        Ok(vec![path_trigger("src/**"), code_trigger("spawn")])
    );
    assert_eq!(
        parse_triggers(Some(too_deep)),
        Ok(default_triggers()),
        "four spaces make a code block, not a fence"
    );
}

#[test]
fn a_bad_line_refuses_the_task_and_names_the_line() {
    let unknown = parse_triggers(Some(
        "Intro\n```kratos-security\npath: src/**\nglob: *.ts\n```",
    ));
    let regex = parse_triggers(Some("```kratos-security\ncode: spawn(\n```"));
    let empty = parse_triggers(Some("```kratos-security\npath:\n```"));
    let glob = parse_triggers(Some("```kratos-security\npath: src/[a\n```"));

    assert_eq!(
        unknown,
        Err(TriggerError::BadLine {
            number: 4,
            problem: LineProblem::Unrecognized,
            text: "glob: *.ts".to_owned()
        })
    );
    assert_eq!(
        unknown.unwrap_err().to_string(),
        "REVIEW.md line 4: expected \"path: <glob>\" or \"code: <regular expression>\": glob: *.ts"
    );
    assert_eq!(
        regex.unwrap_err().to_string(),
        "REVIEW.md line 2: not a valid regular expression: code: spawn("
    );
    assert_eq!(
        empty.unwrap_err().to_string(),
        "REVIEW.md line 2: the trigger is empty: path:"
    );
    assert_eq!(
        glob.unwrap_err().to_string(),
        "REVIEW.md line 2: not a valid glob: path: src/[a"
    );
}

#[test]
fn globs_match_changed_paths_and_patterns_match_added_lines() {
    let triggers = [
        path_trigger("src/providers/**"),
        code_trigger("child_process"),
    ];

    let hits = match_triggers(
        &triggers,
        &[
            file("src/providers/codex.ts", &["const ready = true;"]),
            file(
                "src/ui/view.ts",
                &[
                    "const a = 1;",
                    "import { spawn } from 'node:child_process';  ",
                ],
            ),
            file("docs/notes.md", &["Nothing to see."]),
        ],
    );

    assert_eq!(
        hits,
        Ok(vec![
            TriggerHit::Path {
                glob: "src/providers/**".to_owned(),
                path: "src/providers/codex.ts".to_owned()
            },
            TriggerHit::Code {
                pattern: "child_process".to_owned(),
                path: "src/ui/view.ts".to_owned(),
                line: "import { spawn } from 'node:child_process';".to_owned()
            },
        ])
    );
}

#[test]
fn a_star_stays_inside_one_folder_and_two_stars_cross_folders() {
    let hits = |glob: &str, path: &str| {
        match_triggers(&[path_trigger(glob)], &[file(path, &[])])
            .unwrap()
            .len()
    };

    assert_eq!(hits("*.rs", "main.rs"), 1);
    assert_eq!(hits("*.rs", "src/main.rs"), 0);
    assert_eq!(hits("**/*.rs", "src/deep/main.rs"), 1);
    assert_eq!(hits("**/.env", ".env"), 1);
    assert_eq!(hits("**/.env", "config/.env"), 1);
    assert_eq!(hits("**/.env.*", "config/.env.local"), 1);
}

/// Each default code pattern, a line only it matches, and a neighbouring line it must leave alone.
/// Removing a pattern, or weakening it, fails the row for it.
const FIXTURES: [(usize, &str, &str); 14] = [
    (
        0,
        "const child = spawn('git', args);",
        "const spawned = spawnPoint;",
    ),
    (
        0,
        "await execFile('git', args);",
        "const profile = profileFor(user);",
    ),
    (
        0,
        "import subprocess; subprocess.run(cmd)",
        "let subprocessCount = 0;",
    ),
    (
        1,
        "std::process::Command::new(\"git\").status()?;",
        "let command = String::new();",
    ),
    (
        1,
        "let child = Command::new(\"ls\");",
        "let command = Commands::all();",
    ),
    (
        0,
        "let mut child = cmd.spawn()?;",
        "tokio::spawn(async move { work().await });",
    ),
    (
        2,
        "const response = await fetch(url);",
        "const prefetched = prefetch(url);",
    ),
    (
        3,
        "let body = reqwest::get(url).await?;",
        "let requested = request_count + 1;",
    ),
    (
        3,
        "let stream = TcpStream::connect(addr)?;",
        "let streams = open_streams;",
    ),
    (
        4,
        "std::fs::remove_dir_all(&folder)?;",
        "let text = std::fs::read_to_string(&path)?;",
    ),
    (
        4,
        "let file = File::create(&path)?;",
        "let file = File::open(&path)?;",
    ),
    (
        5,
        "let apiKey = settings.get(\"api_key\");",
        "let keys = lookup.len();",
    ),
    (
        6,
        "if !canUseTool(tool) { deny(); }",
        "let tools = registry.len();",
    ),
    (
        6,
        "std::fs::set_permissions(&path, mode)?;",
        "let permitted = true;",
    ),
];

#[test]
fn each_default_pattern_catches_its_case_and_leaves_the_benign_neighbour_alone() {
    let triggers = default_triggers();
    for (index, risky, benign) in FIXTURES {
        let pattern = DEFAULT_PATTERNS[index];
        let hits = match_triggers(&triggers, &[file("src/x.rs", &[risky])]).unwrap();
        let patterns: Vec<&str> = hits
            .iter()
            .filter_map(|hit| match hit {
                TriggerHit::Code { pattern, .. } => Some(pattern.as_str()),
                TriggerHit::Path { .. } => None,
            })
            .collect();
        assert_eq!(
            patterns,
            [pattern],
            "`{risky}` should hit only pattern {index}"
        );

        let quiet = match_triggers(&triggers, &[file("src/x.rs", &[benign])]).unwrap();
        assert_eq!(quiet, vec![], "`{benign}` is benign");
    }
}

#[test]
fn removing_a_default_pattern_leaves_its_fixture_unmatched() {
    for (index, risky, _) in FIXTURES {
        let without: Vec<Trigger> = default_triggers()
            .into_iter()
            .filter(|trigger| {
                !matches!(trigger, Trigger::Code { pattern } if pattern == DEFAULT_PATTERNS[index])
            })
            .collect();

        let hits = match_triggers(&without, &[file("src/x.rs", &[risky])]).unwrap();

        assert_eq!(
            hits,
            vec![],
            "`{risky}` was caught by something other than pattern {index}"
        );
    }
}

#[test]
fn rust_process_and_network_calls_in_ordinary_source_paths_select_review() {
    let triggers = default_triggers();
    let ordinary = [
        (
            "crates/app/src/run.rs",
            "std::process::Command::new(\"git\").status()",
        ),
        (
            "crates/app/src/net.rs",
            "reqwest::get(\"https://example.com\")",
        ),
    ];

    for (path, line) in ordinary {
        let hits = match_triggers(&triggers, &[file(path, &[line])]).unwrap();
        assert!(!hits.is_empty(), "{line} in {path} selects review");
    }
}

#[test]
fn build_scripts_cargo_config_env_files_and_permission_named_files_select_review_by_path() {
    let triggers = default_triggers();
    for path in [
        "build.rs",
        "crates/app/build.rs",
        ".cargo/config.toml",
        "crates/app/.cargo/config",
        ".env",
        "config/.env.local",
        "src/permissions.rs",
        "src/approvalPolicy.ts",
    ] {
        let hits = match_triggers(&triggers, &[file(path, &[])]).unwrap();
        assert!(!hits.is_empty(), "{path} selects review");
    }
    assert_eq!(
        match_triggers(&triggers, &[file("src/math.rs", &["a + b"])]).unwrap(),
        vec![]
    );
}

#[test]
fn no_more_than_fifty_hits_and_no_more_than_two_hundred_characters_of_a_line_are_kept() {
    let many: Vec<DiffFile> = (0..80)
        .map(|n| file(&format!("src/file{n}.ts"), &["child_process"]))
        .collect();
    let long_line = format!("child_process {}", "x".repeat(500));

    let hits = match_triggers(&[code_trigger("child_process")], &many).unwrap();
    let cut = match_triggers(
        &[code_trigger("child_process")],
        &[file("a.ts", &[&long_line])],
    )
    .unwrap();

    assert_eq!(hits.len(), MAX_HITS);
    let TriggerHit::Code { line, .. } = &cut[0] else {
        panic!("a code hit");
    };
    assert_eq!(line.chars().count(), MAX_HIT_LINE);
}

#[test]
fn a_process_call_after_a_long_ascii_literal_selects_review_with_a_bounded_snippet() {
    let added = format!(
        "let payload = \"{}\"; std::process::Command::new(\"git\").status()?;",
        "x".repeat(2_100)
    );

    let hits = match_triggers(&default_triggers(), &[file("src/run.rs", &[&added])]).unwrap();

    assert_eq!(hits.len(), 1);
    let TriggerHit::Code {
        pattern,
        path,
        line,
    } = &hits[0]
    else {
        panic!("the process code selects review");
    };
    assert_eq!(pattern, DEFAULT_PATTERNS[1]);
    assert_eq!(path, "src/run.rs");
    assert_eq!(line.chars().count(), MAX_HIT_LINE);
    assert!(line.starts_with("let payload = \""));
    assert!(!line.contains("Command::new"));
}

#[test]
fn a_network_call_after_a_long_unicode_literal_selects_review_with_a_bounded_snippet() {
    let added = format!(
        "let payload = \"{}\"; reqwest::get(url).await?;",
        "界🙂".repeat(1_100)
    );

    let hits = match_triggers(&default_triggers(), &[file("src/net.rs", &[&added])]).unwrap();

    assert_eq!(hits.len(), 1);
    let TriggerHit::Code {
        pattern,
        path,
        line,
    } = &hits[0]
    else {
        panic!("the network code selects review");
    };
    assert_eq!(pattern, DEFAULT_PATTERNS[3]);
    assert_eq!(path, "src/net.rs");
    assert_eq!(line.chars().count(), MAX_HIT_LINE);
    assert!(line.contains("界🙂"));
    assert!(line.len() > MAX_HIT_LINE);
    assert!(!line.contains("reqwest::get"));
}

#[test]
fn long_benign_lines_and_disabled_triggers_still_select_no_review() {
    for padding in ["x".repeat(2_100), "界🙂".repeat(1_100)] {
        let benign =
            format!("let payload = \"{padding}\"; tokio::spawn(async move {{ work().await }});");
        let risky = format!("let payload = \"{padding}\"; reqwest::get(url).await?;");

        assert_eq!(
            match_triggers(&default_triggers(), &[file("src/run.rs", &[&benign])]).unwrap(),
            vec![]
        );
        assert_eq!(
            match_triggers(&[], &[file("src/net.rs", &[&risky])]).unwrap(),
            vec![]
        );
    }
}

#[test]
fn a_pattern_that_runs_out_of_budget_on_a_line_counts_as_a_hit_instead_of_hanging_or_failing() {
    // The look-ahead inside the loop forces the backtracking engine, and the two ways to match `a` make
    // it try every split of the line.
    let evil = code_trigger(r"^(?:(a|aa)(?!b))+$");
    let line = format!("{}b", "a".repeat(200));

    let hits = match_triggers(&[evil], &[file("a.ts", &[&line])]).unwrap();

    assert_eq!(hits.len(), 1);
}

#[test]
fn a_stored_trigger_that_no_longer_compiles_is_an_error_naming_it() {
    let broken = match_triggers(&[code_trigger("spawn(")], &[file("a.ts", &["spawn()"])]);

    assert!(matches!(broken, Err(TriggerError::Stale { trigger, .. }) if trigger == "spawn("));
}

#[test]
fn only_a_diffs_added_lines_are_scanned_so_a_removed_dangerous_line_selects_nothing() {
    let diff = Diff::parse(
        "+++ b/src/a.rs\n@@ -1 +1 @@\n-std::process::Command::new(\"rm\");\n+let safe = 1;\n",
    );

    let hits = match_triggers(&default_triggers(), &diff.files).unwrap();

    assert_eq!(hits, vec![]);
}
