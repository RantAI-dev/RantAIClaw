// Post actionable lint failure summary as a PR comment.
// Used by the lint-feedback CI job via actions/github-script.
//
// Required environment variables:
//   CHANGES_RESULT — result of the changes job
//   RUST_CHANGED   — "true" if Rust files changed
//   DOCS_CHANGED   — "true" if docs files changed
//   FMT_RESULT     — result of the fmt job
//   LINT_RESULT    — result of the lint job (clippy and the strict delta gate)
//   DOCS_RESULT    — result of the docs-quality job
//
// The job is informational. Any failed comment write is logged as a warning
// and does not fail the job. A pull request from a fork, whose read-only token
// makes every write fail with a 403, is the usual case. Reads are not wrapped.
//
// When the changes job did not succeed, the other jobs were skipped and say
// nothing about the code, so the script leaves an earlier comment as it is.

module.exports = async ({ github, context, core }) => {
  const owner = context.repo.owner;
  const repo = context.repo.repo;
  const issueNumber = context.payload.pull_request?.number;
  if (!issueNumber) return;

  if (process.env.CHANGES_RESULT !== "success") {
    core.info("Change detection did not succeed. The lint feedback comment is left untouched.");
    return;
  }

  const marker = "<!-- ci-lint-feedback -->";
  const rustChanged = process.env.RUST_CHANGED === "true";
  const docsChanged = process.env.DOCS_CHANGED === "true";
  const fmtResult = process.env.FMT_RESULT || "skipped";
  const lintResult = process.env.LINT_RESULT || "skipped";
  const docsResult = process.env.DOCS_RESULT || "skipped";

  const failures = [];
  if (rustChanged && !["success", "skipped"].includes(fmtResult)) {
    failures.push("`Format (rustfmt)` failed.");
  }
  if (rustChanged && !["success", "skipped"].includes(lintResult)) {
    failures.push("`Lint Gate (Clippy + Strict Delta)` failed.");
  }
  if (docsChanged && !["success", "skipped"].includes(docsResult)) {
    failures.push("`Docs Quality` failed.");
  }

  // Any failed write becomes a warning because this job is informational.
  const write = async (action) => {
    try {
      await action();
    } catch (error) {
      core.warning(`Could not write the lint feedback comment: ${error.message}`);
    }
  };

  const comments = await github.paginate(github.rest.issues.listComments, {
    owner,
    repo,
    issue_number: issueNumber,
    per_page: 100,
  });
  const existing = comments.find((comment) => (comment.body || "").includes(marker));

  if (failures.length === 0) {
    if (existing) {
      await write(() =>
        github.rest.issues.deleteComment({
          owner,
          repo,
          comment_id: existing.id,
        }),
      );
    }
    core.info("No lint/docs gate failures. No feedback comment required.");
    return;
  }

  const runUrl = `${context.serverUrl}/${owner}/${repo}/actions/runs/${context.runId}`;
  const body = [
    marker,
    "### CI lint feedback",
    "",
    "This PR failed one or more fast lint/documentation gates:",
    "",
    ...failures.map((item) => `- ${item}`),
    "",
    "Open the failing logs in this run:",
    `- ${runUrl}`,
    "",
    "Local fix commands:",
    "- `cargo fmt --all`",
    "- `./scripts/ci/rust_quality_gate.sh`",
    "- `./scripts/ci/rust_strict_delta_gate.sh`",
    "- `./scripts/ci/docs_quality_gate.sh`",
    "",
    "After fixes, push a new commit and CI will re-run automatically.",
  ].join("\n");

  if (existing) {
    await write(() =>
      github.rest.issues.updateComment({
        owner,
        repo,
        comment_id: existing.id,
        body,
      }),
    );
  } else {
    await write(() =>
      github.rest.issues.createComment({
        owner,
        repo,
        issue_number: issueNumber,
        body,
      }),
    );
  }
};
