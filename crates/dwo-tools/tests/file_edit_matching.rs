use std::fs;

use dwo_tools::file_edit::apply_patch;

fn run(file: &str, patch: &str) -> Result<String, String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.txt");
    fs::write(&path, file).unwrap();
    let applied = apply_patch(patch, dir.path()).map_err(|error| format!("{error:#}"))?;
    if let Some(failure) = applied.failure {
        return Err(failure.error);
    }
    Ok(fs::read_to_string(&path).unwrap())
}

#[test]
fn hunks_are_matched_forward_only_from_the_previous_hunk() {
    let file = "one\ntwo\nthree\nfour\nfive\n";
    let backwards = "*** Begin Patch\n*** Update File: t.txt\n@@\n four\n-five\n+FIVE\n@@\n one\n-two\n+TWO\n*** End Patch";
    let error = run(file, backwards).unwrap_err();
    assert!(error.contains("did not match target file"), "{error}");

    let forwards = "*** Begin Patch\n*** Update File: t.txt\n@@\n one\n-two\n+TWO\n@@\n four\n-five\n+FIVE\n*** End Patch";
    assert_eq!(
        run(file, forwards).unwrap(),
        "one\nTWO\nthree\nfour\nFIVE\n"
    );
}

#[test]
fn a_blank_line_cannot_be_claimed_as_context_twice() {
    let file = "    }\n\n    async fn next() {\n";
    let double = "*** Begin Patch\n*** Update File: t.txt\n@@\n     }\n \n+    // inserted\n \n     async fn next() {\n*** End Patch";
    let error = run(file, double).unwrap_err();
    assert!(error.contains("did not match target file"), "{error}");

    let fixed = "*** Begin Patch\n*** Update File: t.txt\n@@\n     }\n \n+    // inserted\n+\n     async fn next() {\n*** End Patch";
    assert_eq!(
        run(file, fixed).unwrap(),
        "    }\n\n    // inserted\n\n    async fn next() {\n"
    );
}

#[test]
fn lf_and_crlf_endings_all_match() {
    let body = |newline: &str| {
        [
            "head",
            "",
            "        self.send(SessionRequest::Unload { response }).await?;",
            "        wait.await",
            "            .map_err(|_| X)?",
            "    }",
            "",
            "    async fn send(&self) {",
            "        self.request",
            "            .send(request)",
            "            .await",
            "    }",
            "tail",
        ]
        .join(newline)
            + newline
    };

    let ctx = |line: &str| format!(" {line}");
    let patch = |newline: &str| {
        vec![
            "*** Begin Patch".to_string(),
            "*** Update File: t.txt".to_string(),
            "@@".to_string(),
            ctx("        self.send(SessionRequest::Unload { response }).await?;"),
            ctx("        wait.await"),
            ctx("            .map_err(|_| X)?"),
            ctx("    }"),
            " ".to_string(),
            "+    // inserted".to_string(),
            "+".to_string(),
            ctx("    async fn send(&self) {"),
            ctx("        self.request"),
            ctx("            .send(request)"),
            "*** End Patch".to_string(),
        ]
        .join(newline)
    };

    for file_newline in ["\n", "\r\n"] {
        for patch_newline in ["\n", "\r\n"] {
            let result = run(&body(file_newline), &patch(patch_newline)).unwrap();
            assert!(
                result.contains("    // inserted"),
                "file={file_newline:?} patch={patch_newline:?} -> {result:?}"
            );
            assert!(
                result.contains("        wait.await") && result.contains("tail"),
                "file={file_newline:?} patch={patch_newline:?} -> {result:?}"
            );
        }
    }
}
