<!-- last_synced: 2026-11-21 -->

# Korylus Factory

> [日本語](./README.md) | English

Korylus Factory is a tool for advancing the work plans of the Korylus projects without a person attending to them.
It provides `korylusfactory`, a CLI that advances work plans, and `korylusfactory-web`, a control web app for viewing the progress of each project.

The development tools shared across the Korylus projects (koryluslint and korylusflow) live in [korylus/tools](https://github.com/korylus/tools).
korylusfactory uses the same formats for work plans, review documents, and work reports as korylusflow.

Korylus Factory is developed in Japanese. The Japanese documents are the canonical ones, and the English documents are translations provided as a guide.

## korylusfactory

`korylusfactory` is a CLI (written in Rust) for advancing work plans without a person attending to them.
It picks the work plans approved on the roadmap one at a time and advances each through implementation, review, fixes, and the work report by launching Claude Code and Codex sessions unattended.
When a decision needs a human, it stops that work plan, adds an item to the human queue (a Wikino page), and lets the human know.
It waits for the human to write an answer in the item and publish it, or to resume the plan with a command.

```sh
korylusfactory start
korylusfactory status [--json]
korylusfactory resume [--review-limit <count>]
korylusfactory queue list [--all] [--json]
korylusfactory queue show <page number> [--json]
korylusfactory queue answer <page number> [<answer>...] [--json]
korylusfactory --version
```

Run every subcommand at the project root.

### Configuration file

Write the roadmap URL, the roadmap section that counts as approved, and the sections that count as done in `docs/private/factory/config.json` at the project root.
To use the human queue and the notifications of human waits, also write the project name, where the human queue lives, and the notification command.
To send the state to the control web app, also write its URL.

```json
{
  "roadmap": "https://wikino.app/s/<space>/pages/<page number>",
  "section": "やってる",
  "done_sections": ["リリース待ち", "やった"],
  "project": "<project name>",
  "queue": { "space": "<space>", "topic": 12 },
  "notify": { "command": ["<notification command>", "<argument>"] },
  "web": { "url": "https://<control web app>" }
}
```

| Field           | Description                                                                                                                                                                            |
| --------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `roadmap`       | The URL of the roadmap page. Only pages on `https://wikino.app` are accepted                                                                                                           |
| `section`       | The heading (H2) of the one section that counts as approved. Its items are advanced from top to bottom. Defaults to 「やってる」                                                       |
| `done_sections` | The headings (H2) of the sections that count as done (merged). Defaults to 「リリース待ち」 and 「やった」                                                                             |
| `project`       | The project name, written with letters, digits, hyphens, and underscores. It is passed to human queue items and the notification command. Required when `queue` or `notify` is written |
| `queue`         | Where the human queue lives (`space`: the space identifier, `topic`: the topic number). When omitted, no items are added and plans are resumed only with `resume`                      |
| `notify`        | The command that announces human waits (`command`: an array of the command and its arguments), run without a shell. When omitted, nothing is announced                                 |
| `web`           | The control web app to send the state to (`url`: the web app's URL). The state is sent after writing a state file and every minute. When omitted, nothing is sent                      |

A section missing from the roadmap, an approved section heading that appears more than once in the roadmap, an approved section also listed as done, or an unknown field is treated as a mistake and reported as an error.

The roadmap and work plans are read with the `wikino` command, preferring the draft when there is one.
Read permission on the token used for `wikino auth login <space>` is enough for korylusfactory itself.
Sessions launched by `start` read and write drafts of work plans and work reports.
To use `start`, log in to the `wikino` command used by the sessions with a token that has draft permissions (`draft_page:*`) and permission to read pages and topics (`page:read` and `topic:read`), for each space the sessions work in.
The human queue is read and written with the same permissions.
Do not give the token permission to publish pages (`page:write`).
Publishing is done by a human in the web UI, and publishing an item is what confirms the answer.

### `start`

`start` keeps running until you stop it.
It advances the work plan in progress if there is one, and otherwise the first work plan in the roadmap order (see [`status`](#status)).
When the first work plan in the order cannot be read, it does not skip it and waits for a human to fix the work plan or the roadmap (when the human queue is configured, it adds a notice item).
When there is no work plan to advance, it waits for one to appear on the roadmap.
It rereads the roadmap every 5 minutes.
With `web` configured, you can pause or resume the project ("[Running and pausing](#running-and-pausing)") and switch the agent for each role ("[Agents per role](#agents-per-role)") from the web app.

Tasks that create branches follow korylusflow's [flow of a task](https://github.com/korylus/tools/blob/main/README.en.md#flow-of-a-task).
The branch a task's branch is created from is also chosen in the same order as korylusflow's [base branches](https://github.com/korylus/tools/blob/main/README.en.md#base-branches).
By default, the worker session runs on Claude Code, and the reviewer session runs on Codex for odd-numbered reviews and on Claude Code for even-numbered reviews.
Fixes for a review's findings run on the agent of the slot that did that review.
The agent, model, and effort can be switched per role in the web app ("[Agents per role](#agents-per-role)").
Each session is told which skill to run with which arguments, and returns its result as JSON in a fixed shape.
korylusfactory records a step as done only after checking the result and the repository state (the task branch, commits, uncommitted changes, review documents), and that the only changes to the work plan are the task's checkbox and the implementation decision log.
For tasks the implementer does without creating a branch, the session checks the task in the work plan.
korylusfactory checks the completion conditions before moving on to the next task.
At the end of the work plan, the worker writes the work report, and when it includes the walkthrough, a reviewer reviews the work report.

Progress goes to standard output, and errors and warnings go to standard error.
When it starts and before it begins a work plan, it warns if korylusfactory is outdated (see [Warning when outdated](#warning-when-outdated)).
You cannot run `start` twice for the same project.

### Sessions

Sessions run without asking for confirmation, so use korylusfactory in an isolated environment such as a VM.
Pushing and creating or merging pull requests are forbidden in the instructions, and are also blocked by the following.

- Claude Code's deny rules (`git push`, `gh pr create`, `gh pr merge`). They also deny `korylusfactory resume` and `korylusfactory queue answer`, which resume plans with a human's answer
- A Git entry point placed at the front of the session's `PATH`, which refuses pushes, including `git -C <dir> push`
- A supplementary Git setting that rewrites push destinations to a nonexistent remote helper, and an invalid `GH_TOKEN`

A session has a time limit of 2 hours.
When a session ends without returning a result, exceeds the time limit, or does not meet the completion conditions, korylusfactory waits 5 minutes and redoes the same step, adding the unmet conditions to the instructions.
If the step is still not done after 3 retries, it waits for a human.
When the CLI rejects a model or effort set in the web app and the session ends, it waits for a human without retrying, since retrying would fail the same way ("[Agents per role](#agents-per-role)").

Each session's instructions, result shape, standard output, and standard error are kept under `docs/private/factory/plans/<plan name>/sessions/`.

### Stopping

Stop `start` with Ctrl-C (SIGINT) or SIGTERM.
SIGTERM, which Docker and similar tools send when stopping a service, is handled the same way as Ctrl-C.
The running session is ended, down to the child processes the session started.
The state stays as it was before the session's result was recorded, and the next run redoes the same step, telling the session that it was interrupted.
Until the stopped session has been fully ended, the next `start` cannot run.

In a project whose settings file has `web`, `start` waits for the final state report's HTTP request to finish after the first Ctrl-C or SIGTERM.
`start` still exits if the request fails. It writes each failure reason to standard error once until a report succeeds.
To stop without waiting for the send to finish, press Ctrl-C again or send SIGTERM.

### Human waits and `resume`

When a decision needs a human, korylusfactory stops that work plan, shows the reason, and checks the state file every 5 seconds until the wait is lifted.
When the human queue is configured, every situation is resumed by writing an answer in the human queue item and publishing it (see [Human queue](#human-queue)).
`resume` is the command for resuming from a terminal, for example when Wikino cannot be read or written.

| Situation                                | What the human does                                                         | Resuming                                                       |
| ---------------------------------------- | --------------------------------------------------------------------------- | -------------------------------------------------------------- |
| A task done by a person                  | Do the work and check the task in the work plan                             | `resume`                                                       |
| Preparing a task                         | Clean up uncommitted changes, or fix the task prefix or the base branch     | `resume`                                                       |
| A review finding that needs a human      | Answer the response options in the review document                          | `resume`                                                       |
| The review limit                         | Decide whether to continue or stop                                          | `resume` (`--review-limit` to continue)                        |
| The retry limit                          | Remove the cause of the stop                                                | `resume`                                                       |
| [Agent settings](#agents-per-role)       | Fix the value the CLI rejected on the web app's settings page               | `resume`                                                       |
| A question from a session                | Handle the question the session wrote in its result                         | `resume`                                                       |
| Checking the work report (with feedback) | Add a feedback phase to the work plan                                       | `resume`                                                       |
| Checking the work report (done)          | Create and merge the PR, and move the item to a done section on the roadmap | Not needed (see [Reporting completion](#reporting-completion)) |

`resume` resumes the work plan waiting for a human, according to the situation it was waiting in.

- For a task done by a person and for preparing a task, it picks again from the first unfinished task in the work plan
- For review waits, it reads the human's answers and answers the response options again. When waiting at the review limit, raising the limit with `--review-limit <count>` re-reviews after the fixes, and not raising it moves on after the fixes without re-reviewing
- For the retry limit, a question from a session, and agent settings, it redoes the same step in a new session. For agent settings, it reads the settings from the web app again before launching
- For checking the work report, it confirms that only a feedback phase was added to the work plan, then continues from that task

When the conditions for resuming are not met, it shows the reason and leaves the state unchanged.
It is an error when no work plan, or more than one, is waiting for a human.

### Human queue

A human queue item is a page in the topic set in `queue` in the configuration file.
When a plan starts waiting for a human, `start` adds an item as a draft of a new page.
The item holds the work plan, the situation, the reason, and answer fields that depend on the situation.

| State in Wikino            | Meaning                         |
| -------------------------- | ------------------------------- |
| A draft of a new page      | Waiting for a human             |
| Published, without a draft | Answered                        |
| Published, with a draft    | Sent back (waiting for a human) |

Items waiting for a human appear in Wikino's list of drafts.
Their titles have the form `<project name> - <plan name> - <situation> - <time added>`.
The time is written in the time zone of the environment running `start` (the `TZ` environment variable or the system setting).

Items can also be answered from the web UI on a smartphone.

1. Open the item from Wikino's list of drafts
2. Write the answer in the 「## 回答」 section: check one option in a choice field, and write text under the heading of a free-text field
3. Publish the page

While waiting for a human, `start` reads the item every minute.
When it has been published, `start` applies the answer according to the situation and resumes.

| Situation                                                                                            | Answer fields                                                                                                     |
| ---------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| A review finding that needs a human                                                                  | The response options of each finding. The chosen options are written to the review document, and the fixes follow |
| The review limit                                                                                     | 「上限を延ばして再レビューする」 (raises the limit by one) and 「打ち切って次へ進む」                             |
| A question from a session                                                                            | Free text. The answer is added to the instructions of the session that redoes the same step                       |
| A task done by a person, preparing a task, the retry limit, agent settings, checking the work report | None. Publish after handling it, and the plan resumes under the same conditions as `resume`                       |
| The first work plan cannot be read, branches are not merged                                          | None. No need to publish; `start` cleans up the item once the cause is resolved                                   |

For a review finding, a response not in the options (「その他」) cannot be chosen in the item.
In the review document, the human checks 「その他」 under 「対応方針」 for the finding.
They write the response in the 「回答」 field of the same finding, then run `resume` from a terminal.

When the answer has the wrong form (no option chosen, two options chosen, options rewritten, and so on) or the conditions for resuming are not met, `start` creates a new draft on the published page and writes the reasons in a 「差し戻し」 section at the top of the body.
Fix the draft and publish it again.

When a wait is lifted before its item is published (by `resume` or by detecting completion), `start` keeps the body in the state directory and then deletes the item's draft.
When a human deletes an item's draft, `start` adds the item again.
When Wikino cannot be read or written, `start` shows a warning and retries a minute later.
It does not stop recording the human wait or the task in progress.

### Notifying human waits

Human waits are announced with the notification command and on the terminal.

The notification command (`notify` in the configuration file) runs once when a plan starts waiting for a human (after the item is added) and once each time an answer is sent back.
When the item could not be added, it announces the wait without the URL, and announces it again with the URL once the item is added.
The notice items for a first work plan that cannot be read and for branches that are not merged are announced, with the URL, only once the item is added.
Without the human queue, these two situations are not announced.
The notification command inherits the caller's environment.
korylusfactory passes notification data only through the environment variables listed below.

| Environment variable     | Content                                                                                                                                                |
| ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `KORYLUSFACTORY_PROJECT` | The project name                                                                                                                                       |
| `KORYLUSFACTORY_PLAN`    | The plan name. For the notice that the first work plan cannot be read, the plan name is unknown, so the title of the Wikilink in the roadmap is passed |
| `KORYLUSFACTORY_SCENE`   | The situation. When an answer is sent back, 「<situation> (差し戻し)」                                                                                 |
| `KORYLUSFACTORY_URL`     | The URL that opens the item in the editor. Not passed when no item was added                                                                           |

The part that sends to a destination (such as a push to a smartphone) is prepared on the command's side.
Secrets for the destination are kept by the command, not in the configuration file.
Here is an example of using a script that sends to ntfy as the notification command.

```sh
#!/bin/sh
# ~/.local/bin/notify-factory (the ntfy topic is passed in the NTFY_TOPIC environment variable)
curl -fsS -H "Title: $KORYLUSFACTORY_PROJECT: $KORYLUSFACTORY_SCENE" \
  -H "Click: ${KORYLUSFACTORY_URL:-}" \
  --data-raw "$KORYLUSFACTORY_PLAN" "https://ntfy.sh/$NTFY_TOPIC"
```

```json
{ "notify": { "command": ["notify-factory"] } }
```

The command is stopped after 30 seconds, and failures and timeouts only produce a warning.

When the standard error of `start` is a terminal, `start` shows the current state in the terminal title and rings the bell once when a plan starts waiting for a human.
The title looks like `korylusfactory - <plan name> - タスク 1-2 の実装` during a session and `korylusfactory - 人間待ち - <situation>` while waiting for a human.

### `queue`

`queue` lists and shows human queue items and writes answers to an item's draft.
It only reads and writes Wikino and never writes state files.
From any project, it can read the items of every project in the queue.

| Subcommand                               | Role                                                                                                                             |
| ---------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `queue list [--all]`                     | Shows the items waiting for a human in the order they were added. By default this project's items; `--all` shows every project's |
| `queue show <page number>`               | Shows the item's body and its answer fields (labels and options of choice fields)                                                |
| `queue answer <page number> <answer>...` | Writes answers, in field order, to the item's draft. It does not publish                                                         |

Pass `queue answer` the answers in field order: a label (such as `A`) for a choice field and the answer text for a free-text field.
Pass text starting with `-` after `--`.
It checks the form of the answers before writing, and records a 「回答の書き手: <writer> (<time>)」 line before the answer fields.
The writer is told apart by whether the command runs inside a Claude Code or Codex session.
It does not write to items without answer fields.
The answer is confirmed when a human publishes the page in the web UI.

With `--json`, each of them writes JSON.

### Reporting completion

korylusfactory learns that a work plan is done (merged) from the roadmap, not from a command.
While waiting for the work report to be checked, `start` reads the roadmap every 5 minutes, and when both of the following hold, it records the completion and moves on to the next work plan.

1. A Wikilink to the work plan is in an item of a done section
2. For each repository where tasks created branches, the last task's branch is merged into the default base (develop, or main if there is none) on the remote `origin`

When 1 holds but 2 does not, it shows the branches that are not merged and waits.
A work plan without tasks that create branches is done with 1 alone.
A work plan recorded as done is not advanced even if it is in the approved section of the roadmap.

### `status`

It shows the korylusfactory version, the order of the work plans to advance, and the work plans in progress.
In a project whose configuration file has `web`, if the last setting read shows the project as paused, it shows right after the version that the project is paused and when that setting was read ("[Running and pausing](#running-and-pausing)").
It then shows the last agents per role it read, one slot per line in the form 「作業者: Claude Code・claude-opus-5-5・high」 ("[Agents per role](#agents-per-role)").

The order lists the Wikilinks whose titles end with 「作業計画書」 in the items of the approved section, from top to bottom.
Work plans in other sections (such as 「優先度: 高」) are not included.
Items with only a name (plans not written yet), links to tickets, and nested items are skipped.
When several links point to the same work plan, only the first is kept.
Work plans recorded as done are left out of the order.

Each work plan is shown as one of the following.

| Shown                              | Meaning                                                                      |
| ---------------------------------- | ---------------------------------------------------------------------------- |
| The plan name and URL              | The work plan was read                                                       |
| 作業計画書のページが見つかりません | There is neither a draft nor a page with the link's title                    |
| 作業計画書として読み取れません     | The page exists but cannot be read as a work plan. The URL and reason follow |

Each work plan in progress is shown with its current step and the reason it is waiting for a human.
When an item was added to the human queue, its URL is shown too.
When it is waiting for the work report to be checked, `status` checks for completion and shows the branches that are not merged.

With `--json`, the same content is written as JSON.
The saved settings go in `operation` (`paused`, `agents` for the agents per role, and `read_at`, the time they were read).
`agents` lists every slot, with the default values for slots that have not been saved.
When `web` is unset, `korylusfactory start` has not saved a setting yet, or the saved setting cannot be read, `operation` is `null`.
When the roadmap cannot be read (not logged in, Wikino is down, and so on), it writes the reason to standard error and exits with code 1.

### Warning when outdated

`status` and `start` look up the last commit that changed the Rust code (`crates/`, `Cargo.toml`, `Cargo.lock`) on `main` of the repository it was built from.
If the installed korylusfactory does not contain that commit, they warn you to reinstall with `make install-korylusfactory`.
They only warn and do not reinstall automatically.

Before comparing, they fetch the remote `main`, and compare with the local `main` when fetching fails.
When they cannot check (for example, the source repository is missing), they do not warn and instead show that they could not check and why.

### Where the state is kept

The state is kept in `docs/private/factory/` at the project root.

| File                           | Contents                                                                                                                                                                                                                                                                     |
| ------------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `config.json`                  | The configuration file                                                                                                                                                                                                                                                       |
| `plans/<plan name>/state.json` | The progress of each work plan (current task and step, retry count, reason for waiting for a human, time of completion, human queue items added), a record per task, and a record of each session it launched (role slot, agent, model, effort, start and end times, result) |
| `plans/<plan name>/queue/`     | The bodies of items cleaned up without being published (`<page number>.md`)                                                                                                                                                                                                  |
| `queue.json`                   | The records of notice items added for waits not kept in state files (the first work plan cannot be read, branches are not merged)                                                                                                                                            |
| `queue/`                       | The bodies of notice items that were cleaned up (`<page number>.md`)                                                                                                                                                                                                         |
| `plans/<plan name>/sessions/`  | A record per session                                                                                                                                                                                                                                                         |
| `plans/<plan name>/state.lock` | An empty file used for file locking                                                                                                                                                                                                                                          |
| `start.lock`, `session.lock`   | Empty files used for file locking, to detect a second `start` and the cleanup of a stopped session                                                                                                                                                                           |
| `web-settings.json`            | The last operation setting and agents per role read from the web app, and when they were read ("[Running and pausing](#running-and-pausing)", "[Agents per role](#agents-per-role)")                                                                                         |

The per-task records use the same field names as `tasks` in [korylusflow](https://github.com/korylus/tools/blob/main/README.en.md#korylusflow)'s `flow.json`, so that the work report skills can read them as material.

### `--version`

`--version` shows the version number, the commit it was built from, the build date, and the location of the repository it was built from.
`-V` shows the same on one line, without the repository location.

## Control web app

`korylusfactory-web` is a web app (written in Rust) that receives the state sent by `korylusfactory start` running in each project, and shows it in a list of projects and a page for each project.
You can check from a phone or PC browser which projects are waiting for a human and which are progressing.
A page left open changes without reloading when new state arrives.
Projects whose state has not arrived for more than 5 minutes are shown as 「不通」 (unreachable).
The project settings page lets you switch whether a project runs or is paused ("[Running and pausing](#running-and-pausing)") and the agent for each role ("[Agents per role](#agents-per-role)").

The web app runs on a control VM on exe.dev, only behind its authenticating proxy.
Humans and directors (`korylusfactory start` in each project) are told apart by the headers the exe.dev proxy attaches.

| Principal | How it is identified                                                                                                       | What it can use                                            |
| --------- | -------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| Human     | The request has no `X-Exedev-Source-Vm` header, and the signed-in person's email address (`X-ExeDev-Email`) is in `owners` | The pages                                                  |
| Director  | The name of the VM calling through a peer integration (`X-Exedev-Source-Vm`) is listed in `directors`                      | Only the APIs for reporting state and reading the settings |

No secret for calling the web app is placed on the project VMs.
exe.dev holds the key of the peer integration and resets the calling VM's name, so it cannot be forged.

### Installing on the control VM

Create the control VM on exe.dev.
Connect to it with SSH for steps 1–3, then run step 4 from your local terminal.
The configuration and database paths below use the XDG defaults of the systemd user manager.

1. As in "[Installing](#installing)", prepare mise, Make, and a C compiler, put the `korylus/factory` repository in place, and build the web app at its root

   ```sh
   mise install
   mise exec -- make install-korylusfactory-web
   ```

   It is placed at `~/.local/bin/korylusfactory-web` (change the location with `BINDIR=<directory>`).

2. Copy the sample configuration file, and fill in the email addresses of the people who view the pages, the mapping from director VMs to projects, and the database path

   ```sh
   mkdir -p ~/.config/korylusfactory-web
   cp crates/korylusfactory-web/deploy/config.example.json ~/.config/korylusfactory-web/config.json
   ```

   | Field       | Description                                                                                                                                     |
   | ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
   | `listen`    | The address to listen on. Defaults to `0.0.0.0:8000`                                                                                            |
   | `database`  | The absolute path of the SQLite database file. The default location is `~/.local/state/korylusfactory-web/factory.sqlite3`                      |
   | `owners`    | The email addresses, used to log in to exe.dev, of the people who can view the pages. Write at least one                                        |
   | `directors` | The VMs that can send state (`vm`) and the names of their projects (`project`). The project is decided by this mapping, not by the request body |

   In `database`, expand `~` to the actual path of your home directory.
   If the user manager has `XDG_CONFIG_HOME` set, `%E` in the unit points to that directory.
   Place the configuration file in `korylusfactory-web/config.json` and the unit from the next step in `systemd/user/` under that directory.
   If it has `XDG_STATE_HOME` set, `StateDirectory` creates `korylusfactory-web` under that directory.
   Set `database` to the actual absolute path of `factory.sqlite3` in the resulting directory.

3. Copy the sample systemd user unit, keep it running while you are logged out, and start it

   ```sh
   mkdir -p ~/.config/systemd/user
   cp crates/korylusfactory-web/deploy/korylusfactory-web.service ~/.config/systemd/user/
   ```

   If you installed with `BINDIR`, edit the copied unit before starting it.
   Set the first part of `ExecStart` to the absolute path of the installed `korylusfactory-web` executable.

   ```sh
   sudo loginctl enable-linger "$USER"
   systemctl --user daemon-reload
   systemctl --user enable --now korylusfactory-web
   ```

   The unit creates the database directory before starting.
   Check that it is running with `systemctl --user status korylusfactory-web`, and its output with `journalctl --user -u korylusfactory-web`.

   After a failure, the unit waits five seconds before restarting.
   It allows five starts per minute, including the initial start.
   If it reaches the limit, fix the cause, reset the counter, and start the unit again.

   ```sh
   systemctl --user reset-failed korylusfactory-web
   systemctl --user start korylusfactory-web
   ```

4. From your local terminal, use an SSH key registered with exe.dev to set the proxy port

   Replace `control_vm` with the control VM name and set `control_port` to the port in the control VM's `listen` setting (8000 if you omitted `listen`).

   ```sh
   control_vm=factory-control
   control_port=8000
   ssh exe.dev share port "$control_vm" "$control_port"
   ```

   Keep the proxy private.
   Viewers need both an entry in the app's `owners` and Web access to the exe.dev VM.
   If someone still needs Web access, grant it from your local terminal using the email address they use to sign in.

   ```sh
   viewer_email=viewer@example.com
   ssh exe.dev share add "$control_vm" "$viewer_email"
   ```

   Open `https://<control VM>.exe.xyz/` in a browser and log in to exe.dev to see the list of projects.

### Sending state from each project

Set up the following for each project.

1. From your local terminal, use an SSH key registered with exe.dev to add a peer integration from the project VM to the control VM

   Replace the VM and integration names, and set `control_port` to the port passed to `share port` in step 4.

   ```sh
   control_vm=factory-control
   control_port=8000
   project_vm=annict-factory
   integration_name=annict-control
   ssh exe.dev integrations add http-proxy \
     --name "$integration_name" \
     --target "https://${control_vm}.exe.xyz:${control_port}/" \
     --peer --attach "vm:$project_vm"
   ```

   From the project VM, the web app on the control VM can be called at `https://<integration name>.int.exe.xyz/`.

2. Add the project VM's name and the project's name to `directors` in the control VM's configuration file, and reload it with `systemctl --user restart korylusfactory-web`
3. Write the integration URL in `web` of the project's configuration file ("[Configuration file](#configuration-file)")

   ```json
   {
     "web": { "url": "https://<integration name>.int.exe.xyz" }
   }
   ```

4. On the project VM, reinstall `korylusfactory` and restart the resident service ("[Reinstalling](#reinstalling)").
   For a project not yet running it as a resident service, follow "[Running resident on a project VM](#running-resident-on-a-project-vm)"

When a state report cannot be delivered, `korylusfactory start` prints the reason to standard error.
The operation setting determines whether it starts work. See [Running and pausing](#running-and-pausing).

### Running and pausing

Open the settings page from the button at the top right of a project page, and use the 「このプロジェクトを稼働させる」 (run this project) switch to make the project run or pause.
The setting is kept in the web app's database, and projects without a setting are treated as running.

In a project whose configuration file has `web`, `korylusfactory start` reads the operation setting from the web app before starting a work plan, and before preparing a branch or launching a session.
Any session already running continues to completion after you switch to paused.
While paused, it stops the following and reads the setting again every minute.

| Stopped                                                       | Continued                                                                                                                                            |
| ------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| Starting a work plan, preparing a branch, launching a session | Adding human queue items and receiving answers, showing waits for a human, checking that a work report is done, cleaning up notices, reporting state |

While waiting for a human, the director keeps checking for an answer.
Once the wait clears, the director reads the setting before starting any work.
If the setting is paused, the director waits for it to return to running before launching the next session.
When it reflects the pause, it shows 「ウェブアプリの設定で停止しています。稼働に戻されたら作業を再開します」 (paused by the web app setting; work resumes when switched back to running) once, and sets the terminal title to 「停止中」 (paused).
When switched back to running, it says so the next time it reads the setting and resumes work.

The project list and project pages show 「停止中」 (paused) based on the director's report.
An unreachable director, a wait for a human, or a running session takes precedence over the paused badge.
If a report arrived within the last 5 minutes and its pause state differs from the setting, the project page shows that the change is pending.

`korylusfactory start` saves the value and the time it was read in `docs/private/factory/web-settings.json` when it has no previous setting (the file is missing or unreadable) and when it reads a different value.
Reading the same value again, even after a restart, does not change `read_at`.
When the web app is unreachable or the setting cannot be read, the director prints the reason to standard error and follows the last setting it could read.
If it cannot read the setting from the web app after a restart, it uses the value saved in the file.
It defaults to running if it has no previous setting.
After a failed read, it does not read again until a minute has passed.
It does not repeat a warning with the same reason until the setting can be read again.
See [`status`](#status) for how to check a saved pause from the command line.

Projects whose configuration file does not have `web` never read the setting and always run.

### Agents per role

On the settings page, the 「担当ごとのエージェント」 (agents per role) card switches the agent (Claude Code or Codex), model, and effort for each slot of the roles that launch sessions.
「設定を保存する」 (save settings) saves the three slots together, and the bottom of the card shows when they were last saved.

| Slot                                            | Sessions it runs                                                                          | Default agent |
| ----------------------------------------------- | ----------------------------------------------------------------------------------------- | ------------- |
| Worker (作業者)                                 | Implementing, tasks done without a branch, writing the work report                        | Claude Code   |
| Reviewer, odd-numbered (レビュアー (奇数回目))  | Odd-numbered reviews of a task or work report, and the fixes for those reviews' findings  | Codex         |
| Reviewer, even-numbered (レビュアー (偶数回目)) | Even-numbered reviews of a task or work report, and the fixes for those reviews' findings | Claude Code   |

Leaving the model and effort empty runs each CLI with its own defaults.
The effort is one of `low`, `medium`, `high`, `xhigh`, and `max`.
A model name is at most 64 characters and starts with an ASCII letter or digit.
It can contain ASCII letters and digits and the characters `.`, `_`, `-`, `:`, `[`, and `]`.
A model that breaks these rules is not saved, and the reason is shown below the field.

When both reviewer slots stay set to the same agent and model, any re-review runs in a new session with the model that made the fixes.

`korylusfactory start` reads the agents per role in the same read as the operation setting, and uses them from the next session it launches.
Sessions already running are not changed.
When it launches a session, the progress line shows the agent, model, and effort, as in 「タスク 1-2 の実装を始めます (Claude Code・claude-opus-5-5・high)」.
Slots that have not been saved run with the defaults.
The model and effort are passed as validated values, each as a single argument (`--model` and `--effort` for Claude Code, `-m` and `-c model_reasoning_effort=<effort>` for Codex).

korylusfactory validates the values it reads as well.
For a slot that fails validation, it warns on standard error and uses the last value it read for that slot (or the default if there is none).
It does not repeat the warning for the same slot and reason until the value is fixed.
When the web app is unreachable, it follows the last settings read, as with the operation setting.
The last agents per role read are kept in `agents` in `docs/private/factory/web-settings.json` together with the operation setting, and used when the web app is unreachable right after a restart.
Projects whose configuration file does not have `web` always run with the defaults.

When a session ends because the CLI rejected the specified model or effort, korylusfactory waits for a human with the situation 「エージェントの設定」 (agent settings) instead of retrying.
The reason includes the slot, the values used, and the line of CLI output that rejected the value.
After you fix the value on the web app's settings page and resume, it reads the settings again and redoes the same step in a new session.
Failures it cannot identify as a rejected value are retried as before.
Claude Code warns about an unknown effort and runs with its default, so Claude Code's effort is not covered by this check.

The project page shows the 「最近のセッション」 (recent sessions) card when work plans in progress have session records.
It lists up to five sessions, newest first, with the role, agent, model, duration, and result.
It shows the effort for sessions where an effort was set.
A running session shows elapsed time in place of the duration and 「実行中」 (running) in place of the result.
See [`status`](#status) for how to check the last agents per role read from the command line.

### Running resident on a project VM

On each project VM, run `korylusfactory start` as a compose service (`factory`) that uses the same image and volumes as the development container, and keep it resident by starting the whole development environment detached.
Because every service of the development environment has `restart: unless-stopped`, Docker restarts the development environment and `factory` after a VM reboot or an abnormal exit.
A service stopped with `docker compose stop` stays stopped until someone starts it.

A sample compose configuration is in `crates/korylusfactory-cli/deploy/docker-compose.override.example.yml`.
Copy it into the file layered on top of the project's `docker-compose.yml` (`docker-compose.override.yml`), and adjust the development container's service name, the project root, and the user's home directory to the project.
The sample uses `!reset` to empty the ports, which requires Docker Compose 2.24.0 or later.

| Setting             | Value                                           | Reason                                                                                          |
| ------------------- | ----------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `extends`           | The development container's service (`app`)     | Sessions run with the same tools, logins, and environment variables as the container people use |
| `volumes`           | The same as those layered onto `app`            | `extends` inherits only `app` from `docker-compose.yml`, so layered volumes must be added again |
| `ports`             | Empty (`!reset []`)                             | Avoids competing with `app` for host ports. `factory` does not listen                           |
| `stdin_open`, `tty` | `false`                                         | No terminal. With standard error not a terminal, `start` writes no terminal title or bell       |
| `command`           | `start` of korylusfactory installed in a volume | Uses the version installed from `app` and makes the agent commands available on `PATH`          |
| `init`              | `true`                                          | Keeps `start` from being PID 1, leaving default signal actions and reaping orphans to init      |
| `restart`           | `unless-stopped`                                | Recovers from VM reboots and abnormal exits, but not when stopped by a person                   |
| `stop_grace_period` | `1m`                                            | Waits after SIGTERM for session cleanup and the final state report (see [Stopping](#stopping))  |
| `environment`       | `TZ`                                            | Writes the dates in human queue item titles in the time zone people read                        |

The korylusfactory executable lives in the named volume `korylusfactory-bin`, mounted at `~/.local/korylusfactory/bin` in both `app` and `factory`.
Recreating the containers does not require reinstalling it.

The sample adds `$HOME/.local/bin` to `PATH` in `factory` so the service can run Claude Code and Codex by name.
If a project installs the agent executables elsewhere, adjust the directory added to `PATH`.
For environment variables added to `app` in the override file, put the same references in `factory`'s `environment`.
Pass secret values through environment variables as usual when you start the services in steps 3 and 5.

Start the resident service in the following order.
Run `docker compose` commands on the VM, and everything else inside the development container (`app`).

1. Add the control VM's `directors` entry and the peer integration, and write the integration URL in `web` of the project's configuration file (steps 1 to 3 of "[Sending state from each project](#sending-state-from-each-project)")
2. In the web app, pause the project (see [Running and pausing](#running-and-pausing))
3. On the VM, start `app` with the override file in place so it mounts the named volume.
   Start `factory` in step 5.

   ```sh
   docker compose up -d --no-deps app
   ```

   If the `app` user cannot write to the mount, set the volume owner to that user from inside `app`.

   ```sh
   sudo chown "$(id -u):$(id -g)" "$HOME/.local/korylusfactory/bin"
   ```

   In `app`, move to the root of the `korylus/factory` repository and install korylusfactory into the volume.

   ```sh
   mise exec -- make install-korylusfactory BINDIR="$HOME/.local/korylusfactory/bin"
   ```

4. From `app`, call the settings API through the integration URL and confirm it returns `"paused":true`

   ```sh
   curl -sS https://<integration name>.int.exe.xyz/api/v1/director/settings
   ```

5. On the VM, start the development environment detached.

   ```sh
   docker compose up -d
   ```

   `factory` starts as well.
   In `app`, move to the root of the project being set up and use the executable installed in the volume to confirm that the project is paused.

   ```sh
   "$HOME/.local/korylusfactory/bin/korylusfactory" status
   ```

6. When work may begin, switch the project to running in the web app

The project is paused before the resident service starts because, if `start` begins without reaching the web app, it has no last read settings and proceeds as running.
Once it has read the paused setting, it follows the saved paused setting even if the web app later becomes unreachable.

In a project with a work plan in progress under korylusflow, switch to running only after that work plan is finished.
This keeps korylusflow and korylusfactory from moving branches in the same working tree at the same time.

Once resident, operate it on the VM as follows.

| Task                 | Command                          |
| -------------------- | -------------------------------- |
| View the output      | `docker compose logs -f factory` |
| Stop                 | `docker compose stop factory`    |
| Start after stopping | `docker compose start factory`   |
| Restart              | `docker compose restart factory` |

`docker compose stop factory` sends SIGTERM to `start`, which stops the running session the same way as Ctrl-C before exiting (see [Stopping](#stopping)).
If it has not exited when `stop_grace_period` passes, Docker stops it with SIGKILL.

### Reinstalling

When reinstalling both the web app and korylusfactory, reinstall the web app on the control VM first, then korylusfactory on each project VM.
If an older web app supports only running and pausing, korylusfactory reads that setting and uses the defaults for every agent slot (see [Agents per role](#agents-per-role)).
If the web app predates the settings API, korylusfactory warns that the read failed and follows the last settings it read (see [Running and pausing](#running-and-pausing)).
With a new web app and an old korylusfactory, the agents per role are not used, and the recent sessions card does not appear.

On the control VM, update the repository, build with the same command, and restart.
If you set `BINDIR` when installing, use the same `BINDIR` again.

```sh
mise exec -- make install-korylusfactory-web
systemctl --user restart korylusfactory-web
```

The received state is kept in the database, so the last received state is shown as-is after a restart.
On SIGTERM, the web app closes the notifications to open pages and then stops.

On each project VM, update the `korylus/factory` repository in the development container (`app`), and reinstall into the volume with the same `BINDIR`.

```sh
mise exec -- make install-korylusfactory BINDIR="$HOME/.local/korylusfactory/bin"
```

Reinstalling does not change the running `factory`, which keeps the previous version.
Restart the resident service on the VM to switch to the new version.

```sh
docker compose restart factory
```

## Installing

While advancing work plans, korylusfactory launches git, the `wikino` command, Claude Code, and Codex.
Install each tool and make it available on `PATH`.
Before running `start`, make sure Claude Code and Codex sessions can use the required Korylus skills in the target project.

Install mise, Make, and a C compiler (GCC or Clang) used by Rust as the linker, then run these commands from the root of the `korylus/factory` repository.

```sh
mise install
mise exec -- make install-korylusfactory
```

`make install-korylusfactory` builds it and places it at `~/.local/bin/korylusfactory` (change the location with `BINDIR=<directory>`).
It is built with the Rust version in `mise.toml`.
Use the built executable so that switching branches while `start` is running does not change the code being run.

After installing to the default location, run `~/.local/bin/korylusfactory --version` to see the source commit and repository location.
Add the directory containing the executable to `PATH` to run `korylusfactory` by name.
When the outdated warning appears (see "[Warning when outdated](#warning-when-outdated)"), reinstall with the same command.

## Development

The Cargo workspace (`crates/`) contains the following crates.
From `korylusfactory-cli` to `korylus-workdoc`, dependencies go in one direction: each crate in the table depends on the crate right below it.
`korylusfactory-cli` and `korylusfactory-web` use the types of the state report from `korylusfactory-protocol`.
`korylusfactory-web` depends only on `korylusfactory-protocol`.

| Crate                     | Role                                                                                                                   |
| ------------------------- | ---------------------------------------------------------------------------------------------------------------------- |
| `korylusfactory-cli`      | Parsing command arguments and launching external processes (git, wikino, Claude Code, Codex, the notification command) |
| `korylusfactory-core`     | Deciding which work plan to advance, reading and writing state files, and the state transitions that advance tasks     |
| `korylus-workdoc`         | Reading work documents (work plans, review documents, the roadmap) and reading and writing human queue items           |
| `korylusfactory-web`      | The control web app that lists the state of each project (HTTP server, authentication, SQLite, screens)                |
| `korylusfactory-protocol` | The types of the state reports that `korylusfactory start` and the web app exchange                                    |

External processes are launched only from `korylusfactory-cli`, never from `korylusfactory-core` or `korylus-workdoc`.

Tool versions are kept in `mise.toml`.
From the root of the `korylus/factory` repository, use pnpm to install Oxfmt, which formats Markdown and other files, and the tools that build the web app's CSS and JavaScript (Tailwind CSS, Basecoat, htmx).

```sh
mise install
mise exec -- pnpm install
```

```sh
mise exec -- make install-korylusfactory      # build korylusfactory (to ~/.local/bin/korylusfactory)
mise exec -- make install-korylusfactory-web  # build the web app (to ~/.local/bin/korylusfactory-web)
mise exec -- make test                        # run the tests (Rust and the token generation script)
mise exec -- make lint                        # run clippy
mise exec -- make fmt                         # format with rustfmt + Oxfmt
mise exec -- make fmt-check                   # check formatting
mise exec -- make dev-web                     # start the web app with the development settings
mise exec -- make web-assets                  # rebuild the web app's CSS and JavaScript
```

`.cargo/config.toml` at the root of the repository keeps `cargo add` and `cargo update` from picking versions published less than 3 days ago.
Builds, lints, and tests run with `--locked` (each `make` target and CI) pass even when `Cargo.lock` contains a version published less than 3 days ago.
When you deliberately need a version published less than 3 days ago, such as a security fix, upgrade the crates one at a time.

```sh
CARGO_RESOLVER_INCOMPATIBLE_PUBLISH_AGE=allow cargo update <crate> --precise <version>
```

This command can also select versions published less than 3 days ago for transitive dependencies of the specified crate.
Check the changes to `Cargo.lock` after the update.

Until the upgraded version is 3 days old, do not run `cargo update` without naming a crate.
Doing so can downgrade it to a version that is at least 3 days old.

`make dev-web` uses the settings in `crates/korylusfactory-web/config.dev.json` and listens on port 8080.
Because the development environment has no exe.dev proxy, it treats every request as coming from the development identity written in the settings.

The web app embeds the CSS and JavaScript in `crates/korylusfactory-web/static/` into the executable.
When you change the templates (`templates/`), the tokens (`assets/tokens.json`), or the CSS in `assets/`, rebuild them with `make web-assets` and commit the changes to `static/` and `assets/tokens.css` along with them.

CI runs `make web-assets-check`, `make fmt-check`, `make lint`, and `make test` on pushes to `main` and on pull requests.
`make web-assets-check` fails when the rebuilt CSS and JavaScript differ from the committed files.
