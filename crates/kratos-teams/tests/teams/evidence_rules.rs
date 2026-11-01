//! The evidence rules that decide which findings Kratos believes: locations, quotes, commands, and
//! the diff parser that says which lines a round changed. Ported from `teams-evidence.test.ts`.

use kratos_protocol::CommandResult;
use kratos_teams::evidence::{
    CommandJudgment, Diff, DropWhy, EvidenceCheck, GIT_DIFF_ARGS, LineRange, LocationProblem,
    RerunVerdict, Seen, Start, finish_check, judge_command, location_problem, quote_appears, rerun,
    start_check,
};
use kratos_teams::reports::Location;

use crate::support::*;

fn location(path: &str, line: u32) -> Location {
    Location {
        path: path.to_owned(),
        line,
    }
}

fn exited(exit_code: i32, stdout: &str, stderr: &str) -> CommandResult {
    CommandResult::Exited {
        exit_code,
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
    }
}

#[test]
fn a_bug_is_confirmed_only_when_its_command_ran_and_exited_non_zero() {
    assert_eq!(
        judge_command(&exited(1, "FAIL a.test.ts", "boom")),
        CommandJudgment::Fails {
            output: "FAIL a.test.ts\nboom".to_owned()
        }
    );
    assert_eq!(
        judge_command(&exited(0, "ok", "")),
        CommandJudgment::Passes {
            output: "ok".to_owned()
        }
    );
    assert_eq!(
        judge_command(&times_out()),
        CommandJudgment::Inconclusive {
            why: DropWhy::CommandTimedOut,
            output: "still running".to_owned()
        }
    );
    assert_eq!(
        judge_command(&refused()),
        CommandJudgment::Inconclusive {
            why: DropWhy::CommandRefused,
            output: "write outside the roots".to_owned()
        }
    );
}

#[test]
fn a_crashed_process_that_reports_a_negative_status_still_confirms_the_bug() {
    // On Windows a crashed process reports its NTSTATUS, which reads as a negative number.
    assert!(matches!(
        judge_command(&exited(-1073741819, "", "")),
        CommandJudgment::Fails { .. }
    ));
}

#[test]
fn the_output_kept_is_the_tail_so_a_noisy_command_cannot_flood_the_record() {
    let noisy = format!("{}END", "x".repeat(10_000));

    let CommandJudgment::Fails { output } = judge_command(&exited(1, &noisy, "")) else {
        panic!("a failing command fails");
    };

    assert_eq!(output.chars().count(), 4000);
    assert!(output.ends_with("END"));
}

#[test]
fn a_rerun_that_could_not_run_is_inconclusive_and_the_exit_code_decides_otherwise() {
    assert_eq!(rerun(&exited(1, "", "")).verdict, RerunVerdict::Fails);
    assert_eq!(rerun(&exited(0, "", "")).verdict, RerunVerdict::Passes);
    assert_eq!(rerun(&times_out()).verdict, RerunVerdict::Inconclusive);
    assert_eq!(rerun(&refused()).verdict, RerunVerdict::Inconclusive);
}

#[test]
fn a_location_counts_only_when_a_relative_path_names_an_existing_line() {
    let text = Some("one\ntwo\nthree\n");

    assert_eq!(location_problem(&location("src/a.ts", 3), text), None);
    assert_eq!(location_problem(&location(".\\src\\a.ts", 1), text), None);
    assert_eq!(
        location_problem(&location("src/a.ts", 4), text),
        Some(LocationProblem::NoSuchLine {
            path: "src/a.ts".to_owned(),
            line: 4,
            lines: 3
        })
    );
    assert!(matches!(
        location_problem(&location("src/a.ts", 0), text),
        Some(LocationProblem::NoSuchLine { line: 0, .. })
    ));
    assert_eq!(
        location_problem(&location("src/a.ts", 1), None),
        Some(LocationProblem::NoSuchFile("src/a.ts".to_owned()))
    );
    assert_eq!(
        location_problem(&location("src/../../etc/passwd", 1), text),
        Some(LocationProblem::LeavesRepository(
            "src/../../etc/passwd".to_owned()
        ))
    );
    assert_eq!(
        location_problem(&location("C:/Windows/win.ini", 1), text),
        Some(LocationProblem::NotRelative(
            "C:/Windows/win.ini".to_owned()
        ))
    );
    assert_eq!(
        location_problem(&location("/etc/passwd", 1), text),
        Some(LocationProblem::NotRelative("/etc/passwd".to_owned()))
    );
}

#[test]
fn a_final_newline_does_not_add_a_line_and_an_empty_file_has_none() {
    assert_eq!(location_problem(&location("a", 2), Some("one\ntwo")), None);
    assert!(location_problem(&location("a", 3), Some("one\ntwo\n")).is_some());
    assert!(location_problem(&location("a", 1), Some("")).is_some());
}

#[test]
fn a_quote_counts_when_it_is_long_enough_and_appears_in_a_source_whatever_the_whitespace() {
    let agents_md = "Every   public function\nneeds a test.";

    assert!(quote_appears("public function needs a test", &[agents_md]));
    assert!(quote_appears("needs a test", &[agents_md]));
    assert!(
        !quote_appears("a test.", &[agents_md]),
        "too short to mean anything"
    );
    assert!(
        !quote_appears("every public function needs a test", &[agents_md]),
        "case matters"
    );
}

#[test]
fn the_location_is_checked_before_a_command_runs() {
    let finding = bug("null-deref", "cmd1");
    let seen = Seen {
        file_text: None,
        sources: &[],
        diff: None,
    };

    let Start::Done(EvidenceCheck::Dropped { reason }) = start_check(&finding, &seen) else {
        panic!("a made-up location is dropped without running anything");
    };

    assert_eq!(reason.why, DropWhy::Location);
    assert_eq!(
        reason.detail,
        "src/a.rs does not exist in the reviewed commit."
    );
}

#[test]
fn a_bug_with_a_real_location_asks_for_its_command_and_a_quote_is_decided_at_once() {
    let seen = Seen {
        file_text: Some("one\ntwo\nthree\n"),
        sources: &["Every public function needs a test."],
        diff: None,
    };

    assert_eq!(
        start_check(&bug("null-deref", "cmd1"), &seen),
        Start::RunCommand("cmd1".to_owned())
    );
    assert_eq!(
        start_check(
            &requirement("untested", "Every public function needs a test."),
            &seen
        ),
        Start::Done(EvidenceCheck::Confirmed {
            inside_new_diff: true,
            output: None
        })
    );
    let Start::Done(EvidenceCheck::Dropped { reason }) =
        start_check(&requirement("spec", "no such rule anywhere"), &seen)
    else {
        panic!("a quote nobody wrote is dropped");
    };
    assert_eq!(reason.why, DropWhy::Quote);
}

#[test]
fn a_nit_a_design_opinion_and_a_pre_existing_problem_have_nothing_to_check() {
    let seen = Seen {
        file_text: None,
        sources: &[],
        diff: None,
    };

    for finding in [nit("naming"), design("layering"), pre_existing("old")] {
        assert_eq!(start_check(&finding, &seen), Start::NothingToCheck);
    }
}

#[test]
fn a_command_that_passes_times_out_or_is_refused_drops_the_finding_with_that_reason() {
    let finding = bug("null-deref", "cmd1");
    let why = |result: CommandResult| match finish_check(&finding, &result, None) {
        EvidenceCheck::Dropped { reason } => reason.why,
        other => panic!("{other:?}"),
    };

    assert_eq!(why(exited(0, "", "")), DropWhy::CommandPassed);
    assert_eq!(why(times_out()), DropWhy::CommandTimedOut);
    assert_eq!(why(refused()), DropWhy::CommandRefused);
    assert_eq!(
        finish_check(&finding, &exited(1, "FAIL", ""), None),
        EvidenceCheck::Confirmed {
            inside_new_diff: true,
            output: Some("FAIL".to_owned())
        }
    );
}

#[test]
fn a_confirmed_finding_is_inside_the_diff_only_when_a_changed_line_covers_it() {
    let finding = bug("null-deref", "cmd1");
    let elsewhere = Diff::parse("+++ b/src/a.rs\n@@ -9 +9 @@\n-x\n+y\n");
    let here = Diff::parse("+++ b/src/a.rs\n@@ -3 +3 @@\n-x\n+y\n");

    let inside = |diff: &Diff| match finish_check(&finding, &exited(1, "", ""), Some(diff)) {
        EvidenceCheck::Confirmed {
            inside_new_diff, ..
        } => inside_new_diff,
        other => panic!("{other:?}"),
    };

    assert!(inside(&here));
    assert!(!inside(&elsewhere));
}

/// Real `git diff -U0` output: a deletion-only hunk, a new file whose first line starts with "++", a
/// deleted file, and a name with a space, which git ends with a tab.
const UNIFIED_ZERO: &str = "diff --git a/a.ts b/a.ts
index b2f931a..16beaec 100644
--- a/a.ts
+++ b/a.ts
@@ -2 +2 @@ one
-two
+TWO
@@ -4 +3,0 @@ three
-four
@@ -5,0 +5 @@ five
+six
diff --git a/new.ts b/new.ts
new file mode 100644
index 0000000..f0b257e
--- /dev/null
+++ b/new.ts
@@ -0,0 +1,2 @@
+++ looks like a header
+plain
diff --git a/old.ts b/old.ts
deleted file mode 100644
index 286c5f5..0000000
--- a/old.ts
+++ /dev/null
@@ -1 +0,0 @@
-gone
diff --git a/with space.ts b/with space.ts
index 587be6b..b77b4eb 100644
--- a/with space.ts\t
+++ b/with space.ts\t
@@ -1,0 +2 @@ x
+y
";

fn ranges(diff: &Diff, path: &str) -> Vec<(u32, u32)> {
    diff.files
        .iter()
        .find(|file| file.path == path)
        .map(|file| file.ranges.iter().map(|r| (r.first, r.last)).collect())
        .unwrap_or_default()
}

fn added(diff: &Diff, path: &str) -> Vec<String> {
    diff.files
        .iter()
        .find(|file| file.path == path)
        .map(|file| file.added.clone())
        .unwrap_or_default()
}

#[test]
fn the_diff_parser_reads_changed_ranges_and_added_lines_from_git_diff_u0() {
    let text = UNIFIED_ZERO.replace("\\t", "\t");

    let diff = Diff::parse(&text);

    assert_eq!(ranges(&diff, "a.ts"), [(2, 2), (3, 4), (5, 5)]);
    assert_eq!(ranges(&diff, "new.ts"), [(1, 2)]);
    assert_eq!(ranges(&diff, "with space.ts"), [(2, 2)]);
    assert_eq!(added(&diff, "a.ts"), ["TWO", "six"]);
    assert_eq!(added(&diff, "new.ts"), ["++ looks like a header", "plain"]);
    assert_eq!(added(&diff, "with space.ts"), ["y"]);
    assert!(
        diff.files.iter().all(|file| file.path != "old.ts"),
        "a deleted file has no new side"
    );
    assert!(diff.touches(&location("a.ts", 4)));
    assert!(!diff.touches(&location("./a.ts", 1)));
    assert!(!diff.touches(&location("other.ts", 1)));
}

#[test]
fn the_diff_parser_counts_context_lines_so_a_diff_with_context_reads_like_one_without() {
    // The same change as the first hunk above, made with -U1: a context line before and after.
    let with_context = "diff --git a/a.ts b/a.ts
--- a/a.ts
+++ b/a.ts
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
diff --git a/b.ts b/b.ts
--- a/b.ts
+++ b/b.ts
@@ -7 +7 @@
-x
+y
";

    let diff = Diff::parse(with_context);

    assert_eq!(ranges(&diff, "a.ts"), [(2, 2)]);
    assert_eq!(
        ranges(&diff, "b.ts"),
        [(7, 7)],
        "the next file starts where the first hunk ends, not inside it"
    );
    assert_eq!(added(&diff, "a.ts"), ["TWO"]);
}

#[test]
fn a_context_line_between_two_changes_splits_them_into_two_ranges() {
    let diff = Diff::parse("+++ b/a.ts\n@@ -1,4 +1,4 @@\n-a\n+A\n b\n c\n-d\n+D\n");

    assert_eq!(ranges(&diff, "a.ts"), [(1, 1), (4, 4)]);
}

#[test]
fn a_changed_line_that_looks_like_a_diff_header_is_added_text_and_never_a_file() {
    let diff = Diff::parse("+++ b/real.ts\n@@ -0,0 +1,2 @@\n+++ b/fake.ts\n+@@ -1 +1 @@\n");

    let paths: Vec<&str> = diff.files.iter().map(|file| file.path.as_str()).collect();
    assert_eq!(paths, ["real.ts"]);
    assert_eq!(added(&diff, "real.ts"), ["++ b/fake.ts", "@@ -1 +1 @@"]);
}

#[test]
fn the_no_newline_marker_belongs_to_its_line_and_counts_for_nothing() {
    let diff = Diff::parse(
        "+++ b/a.ts\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n",
    );

    assert_eq!(ranges(&diff, "a.ts"), [(1, 1)]);
    assert_eq!(added(&diff, "a.ts"), ["new"]);
}

#[test]
fn a_quoted_path_with_non_ascii_bytes_decodes_as_one_name() {
    // `core.quotePath=true` writes each byte of "é.rs" as an octal escape. The diff arguments turn that
    // off, and the parser still reads the escapes if a caller forgot.
    let diff = Diff::parse("+++ \"b/\\303\\251.rs\"\n@@ -0,0 +1 @@\n+x\n");

    assert_eq!(diff.files[0].path, "é.rs");
}

#[test]
fn every_diff_the_rules_read_is_made_without_quoting_non_ascii_paths() {
    let args = GIT_DIFF_ARGS.join(" ");

    assert!(args.starts_with("-c core.quotePath=false diff"));
    assert!(args.contains("--no-ext-diff"));
    assert!(args.contains("--dst-prefix=b/"));
}

#[test]
fn a_truncated_diff_ends_with_the_text_instead_of_failing() {
    let diff = Diff::parse("+++ b/a.ts\n@@ -1,5 +1,5 @@\n-a\n+b\n");

    assert_eq!(ranges(&diff, "a.ts"), [(1, 1)]);
    assert_eq!(diff.files[0].ranges, [LineRange { first: 1, last: 1 }]);
}
