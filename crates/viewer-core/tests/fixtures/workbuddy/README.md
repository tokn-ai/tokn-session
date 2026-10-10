# WorkBuddy fixtures

These fixtures were generated with WorkBuddy AI 5.4.2's bundled CodeBuddy
Code 2.132 runtime in headless mode. The runtime used the OpenAI-compatible
endpoint `http://localhost:4141/v1` with `deepseek-v4-flash`.

## Session histories

The JSONL records under `projects/fixture-workspace` cover:

- `wb-chat-basic`: a one-turn text conversation
- `wb-file-read-local`: a successful `Read` tool call and result
- `wb-shell-command`: two `Bash` calls and results
- `wb-multiturn`: a resumed two-turn conversation
- `wb-file-read`: an authentication failure

The histories retain the provider-native records. The generated temporary
workspace path was replaced with `/fixture/workspace`; no home-directory path,
username, account identifier, credential, or other personal data is retained.

## Database

`workbuddy.db` uses the genuine schema initialized by WorkBuddy AI 5.4.2. Its
four catalog rows are synthetic because WorkBuddy's desktop catalog insertion
requires a non-empty authenticated user ID, while the headless custom-model
sessions do not. The rows follow the packaged desktop insertion behavior and
match the four successful JSONL session IDs, titles, modes, models, permission
modes, and timestamps.

The synthetic catalog uses `/fixture/workspace` and `fixture-user`. The database
was vacuumed after sanitization so replaced values do not remain in free pages.
