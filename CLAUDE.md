# Claude Code repository instructions

These instructions apply to the entire repository.

## Rust test placement

- Never place Rust tests in production source files.
- Do not add `#[cfg(test)]`, `#[test]`, namespaced test attributes such as
  `#[tokio::test]`, or inline `mod tests` modules under `crates/**/src/` or
  `examples/**/src/`.
- Put every Rust test in a dedicated `tests/` directory. Use
  `crates/<crate>/tests/` for crate or binary integration tests and
  `tests/code/` for cross-crate harness tests.
- Exercise behavior through public APIs or a compiled executable. CLI tests
  must launch the compiled binary as a child process and use an isolated
  temporary working directory rather than a production source directory.
- Keep test-only fixtures and support data under the relevant `tests/`
  directory.
- If behavior is difficult to test externally, improve the public boundary or
  extract reusable production logic instead of adding inline tests.

## Commit messages

Every new commit created by an agent must use this structure:

```text
feat(scope): short description

- longer description of the change
- longer description of the impact or verification
```

- The type must be exactly `feat`, `fix`, or `chore`.
- A lowercase, meaningful scope in parentheses is required.
- Keep the subject concise and separate it from the body with one blank line.
- Include at least two non-empty `- ` bullet lines in the body.
- Do not create subject-only commits or unscoped subjects such as `fix: ...`.
- Inspect the staged diff before committing so the subject and bullets describe
  the complete commit accurately.
- Do not amend or force-push an already published commit solely to retrofit
  this format unless the user explicitly authorizes rewriting history.
