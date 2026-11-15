use super::*;

#[test]
fn a_passing_unittest_run_is_complete() {
    let out =
        "...\n----------------------------------------------------------------------\nRan 3 tests in 0.004s\n\nOK\n";
    assert_eq!(parse(Runner::Unittest, out), Outcome { complete: true, ran: Some(3), failures: vec![] });
}

#[test]
fn unittest_and_django_failures_and_errors_are_named() {
    let out = "F.E
======================================================================
ERROR: test_b (pkg.tests.test_x.T.test_b)
----------------------------------------------------------------------
Traceback (most recent call last):
======================================================================
FAIL: test_a (pkg.tests.test_x.T.test_a)
----------------------------------------------------------------------
AssertionError

----------------------------------------------------------------------
Ran 3 tests in 0.010s

FAILED (failures=1, errors=1)
";
    for runner in [Runner::Unittest, Runner::Django] {
        let outcome = parse(runner, out);
        assert!(outcome.complete);
        assert_eq!(outcome.ran, Some(3));
        assert_eq!(outcome.failures, ["test_b (pkg.tests.test_x.T.test_b)", "test_a (pkg.tests.test_x.T.test_a)"]);
    }
}

#[test]
fn a_module_that_does_not_import_is_a_failure() {
    let out = "E
======================================================================
ERROR: test_gone (unittest.loader._FailedTest.test_gone)
----------------------------------------------------------------------
ImportError: Failed to import test module: test_gone

----------------------------------------------------------------------
Ran 1 test in 0.000s

FAILED (errors=1)
";
    let outcome = parse(Runner::Unittest, out);
    assert!(outcome.complete);
    assert_eq!(outcome.failures, ["test_gone (unittest.loader._FailedTest.test_gone)"]);
}

#[test]
fn a_unittest_run_cut_short_is_not_complete() {
    assert!(!parse(Runner::Unittest, "..\nTraceback (most recent call last):\nKilled\n").complete);
    // `Ran` without its status line is not the summary either.
    assert!(!parse(Runner::Unittest, "Ran 2 tests in 0.1s\n").complete);
}

#[test]
fn pytest_counts_and_failures_come_from_its_summary() {
    let out = "tools/tests/ci/test_a.py ..F
=========================== short test summary info ============================
FAILED tools/tests/ci/test_a.py::TestA::test_c - AssertionError: x
ERROR tools/tests/ci/test_b.py - ImportError
=============== 1 failed, 2 passed, 1 warning, 1 error in 0.52s ================
";
    assert_eq!(
        parse(Runner::Pytest, out),
        Outcome {
            complete: true,
            ran: Some(4),
            failures: vec!["tools/tests/ci/test_a.py::TestA::test_c".into(), "tools/tests/ci/test_b.py".into()],
        }
    );
}

#[test]
fn pytest_without_its_closing_line_did_not_finish() {
    assert!(!parse(Runner::Pytest, "tools/tests/ci/test_a.py ..\n").complete);
    assert!(
        !parse(Runner::Pytest, "============================= test session starts ==============================\n")
            .complete
    );
    assert_eq!(parse(Runner::Pytest, "============ no tests ran in 0.01s ============\n").ran, Some(0));
}

#[test]
fn each_runner_puts_extra_arguments_before_the_targets() {
    assert_eq!(
        Runner::Pytest.command(&["-x".into()], &["a.py".into()]),
        ["python", "-m", "pytest", "-p", "no:cacheprovider", "-rfE", "-x", "a.py"]
    );
    assert_eq!(Runner::Django.command(&[], &["core".into()]), ["python", "manage.py", "test", "--noinput", "core"]);
    assert_eq!(Runner::Unittest.command(&[], &["a.b".into(), "c".into()]), ["python", "-m", "unittest", "a.b", "c"]);
    assert_eq!(Runner::parse("unittest"), Some(Runner::Unittest));
    assert_eq!(Runner::parse("nose"), None);
}
