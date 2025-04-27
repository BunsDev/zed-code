mod edit_parser;

use crate::{Template, Templates};
use anyhow::{Result, anyhow};
use edit_parser::EditParser;
use futures::{Stream, StreamExt, TryStreamExt, stream};
use gpui::{AsyncApp, Entity};
use language::{Anchor, Bias, Buffer, BufferSnapshot};
use language_model::{
    LanguageModel, LanguageModelRequest, LanguageModelRequestMessage, MessageContent, Role,
};
use project::{Project, ProjectPath};
use serde::Serialize;
use smallvec::{SmallVec, smallvec};
use std::{ops::Range, path::PathBuf, sync::Arc};

#[derive(Serialize)]
pub struct EditAgentTemplate {
    path: Option<PathBuf>,
    file_content: String,
    instructions: String,
}

impl Template for EditAgentTemplate {
    const TEMPLATE_NAME: &'static str = "edit_agent.hbs";
}

pub struct EditAgent {
    project: Entity<Project>,
    model: Arc<dyn LanguageModel>,
    templates: Arc<Templates>,
}

impl EditAgent {
    pub fn new(
        model: Arc<dyn LanguageModel>,
        project: Entity<Project>,
        templates: Arc<Templates>,
    ) -> Self {
        EditAgent {
            project,
            model,
            templates,
        }
    }

    pub async fn interpret(
        &self,
        buffer: Entity<Buffer>,
        instructions: String,
        cx: &mut AsyncApp,
    ) -> Result<impl Stream<Item = Result<(Range<Anchor>, String)>>> {
        let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot())?;
        let path = cx.update(|cx| snapshot.resolve_file_path(cx, true))?;
        // todo!("move to background")
        let prompt = EditAgentTemplate {
            path,
            file_content: snapshot.text(),
            instructions,
        }
        .render(&self.templates)?;
        let request = LanguageModelRequest {
            messages: vec![LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(prompt)],
                cache: false,
            }],
            ..Default::default()
        };
        let mut parser = EditParser::new();
        let stream = self.model.stream_completion_text(request, cx).await?.stream;
        Ok(stream.flat_map(move |chunk| {
            let mut edits = SmallVec::new();
            let mut error = None;
            let snapshot = snapshot.clone();
            match chunk {
                Ok(chunk) => {
                    edits = parser.push(&chunk);
                    // print!("{}", chunk);
                }
                Err(err) => error = Some(Err(anyhow!(err))),
            }
            stream::iter(
                edits
                    .into_iter()
                    .map(move |edit| {
                        let range = Self::resolve_location(&snapshot, &edit.old_text);
                        Ok((range, edit.new_text))
                    })
                    .chain(error),
            )
        }))
    }

    fn resolve_location(buffer: &BufferSnapshot, search_query: &str) -> Range<Anchor> {
        const INSERTION_COST: u32 = 3;
        const DELETION_COST: u32 = 10;
        const WHITESPACE_INSERTION_COST: u32 = 1;
        const WHITESPACE_DELETION_COST: u32 = 1;

        let buffer_len = buffer.len();
        let query_len = search_query.len();
        let mut matrix = SearchMatrix::new(query_len + 1, buffer_len + 1);
        let mut leading_deletion_cost = 0_u32;
        for (row, query_byte) in search_query.bytes().enumerate() {
            let deletion_cost = if query_byte.is_ascii_whitespace() {
                WHITESPACE_DELETION_COST
            } else {
                DELETION_COST
            };

            leading_deletion_cost = leading_deletion_cost.saturating_add(deletion_cost);
            matrix.set(
                row + 1,
                0,
                SearchState::new(leading_deletion_cost, SearchDirection::Diagonal),
            );

            for (col, buffer_byte) in buffer.bytes_in_range(0..buffer.len()).flatten().enumerate() {
                let insertion_cost = if buffer_byte.is_ascii_whitespace() {
                    WHITESPACE_INSERTION_COST
                } else {
                    INSERTION_COST
                };

                let up = SearchState::new(
                    matrix.get(row, col + 1).cost.saturating_add(deletion_cost),
                    SearchDirection::Up,
                );
                let left = SearchState::new(
                    matrix.get(row + 1, col).cost.saturating_add(insertion_cost),
                    SearchDirection::Left,
                );
                let diagonal = SearchState::new(
                    if query_byte == *buffer_byte {
                        matrix.get(row, col).cost
                    } else {
                        matrix
                            .get(row, col)
                            .cost
                            .saturating_add(deletion_cost + insertion_cost)
                    },
                    SearchDirection::Diagonal,
                );
                matrix.set(row + 1, col + 1, up.min(left).min(diagonal));
            }
        }

        // Traceback to find the best match
        let mut best_buffer_end = buffer_len;
        let mut best_cost = u32::MAX;
        for col in 1..=buffer_len {
            let cost = matrix.get(query_len, col).cost;
            if cost < best_cost {
                best_cost = cost;
                best_buffer_end = col;
            }
        }

        let mut query_ix = query_len;
        let mut buffer_ix = best_buffer_end;
        while query_ix > 0 && buffer_ix > 0 {
            let current = matrix.get(query_ix, buffer_ix);
            match current.direction {
                SearchDirection::Diagonal => {
                    query_ix -= 1;
                    buffer_ix -= 1;
                }
                SearchDirection::Up => {
                    query_ix -= 1;
                }
                SearchDirection::Left => {
                    buffer_ix -= 1;
                }
            }
        }

        let mut start = buffer.offset_to_point(buffer.clip_offset(buffer_ix, Bias::Left));
        start.column = 0;
        let mut end = buffer.offset_to_point(buffer.clip_offset(best_buffer_end, Bias::Right));
        if end.column > 0 {
            end.column = buffer.line_len(end.row);
        }

        buffer.anchor_after(start)..buffer.anchor_before(end)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum SearchDirection {
    Up,
    Left,
    Diagonal,
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SearchState {
    cost: u32,
    direction: SearchDirection,
}

impl SearchState {
    fn new(cost: u32, direction: SearchDirection) -> Self {
        Self { cost, direction }
    }
}

struct SearchMatrix {
    cols: usize,
    data: Vec<SearchState>,
}

impl SearchMatrix {
    fn new(rows: usize, cols: usize) -> Self {
        SearchMatrix {
            cols,
            data: vec![SearchState::new(0, SearchDirection::Diagonal); rows * cols],
        }
    }

    fn get(&self, row: usize, col: usize) -> SearchState {
        self.data[row * self.cols + col]
    }

    fn set(&mut self, row: usize, col: usize, cost: SearchState) {
        self.data[row * self.cols + col] = cost;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use client::{Client, UserStore};
    use collections::HashSet;
    use fs::FakeFs;
    use gpui::{AppContext, TestAppContext};
    use indoc::indoc;
    use language_model::LanguageModelRegistry;
    use rand::prelude::*;
    use reqwest_client::ReqwestClient;
    use serde_json::json;
    use std::{
        fmt::Write as _,
        io::Write as _,
        path::Path,
        sync::{
            atomic::{self, AtomicUsize},
            mpsc,
        },
    };
    use util::path;

    #[derive(Clone)]
    struct Eval {
        input_path: PathBuf,
        input_content: String,
        instructions: String,
        expected_output_variants: Vec<String>,
    }

    #[gpui::test]
    async fn test_basic(cx: &mut TestAppContext) {
        let test = agent_test(cx).await;
        apply_edits(
            "root/blame.rs",
            indoc! {"
                struct User {
                    id: u32,
                    name: String,
                }

                impl User {
                    pub fn new(id: u32, name: String) -> Self {
                        User { id, name }
                    }

                    pub fn id(&self) -> u32 {
                        self.id
                    }

                    pub fn name(&self) -> &str {
                        &self.name
                    }
                }
            "},
            indoc! {"
                Introduce a new field `age: u8`, add it to the constructor
                and also add a getter method for it.
            "},
            &test,
            cx,
        )
        .await;
    }

    #[test]
    fn test_delete_run_git_blame() {
        eval(
            100,
            0.9,
            Eval {
                input_path: "root/blame.rs".into(),
                input_content: include_str!("fixtures/delete_run_git_blame/before.rs").into(),
                instructions: indoc! {r#"
                    Let's delete the `run_git_blame` function while keeping all other code intact:

                    // ... existing code ...
                    const GIT_BLAME_NO_COMMIT_ERROR: &str = "fatal: no such ref: HEAD";
                    const GIT_BLAME_NO_PATH: &str = "fatal: no such path";

                    #[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq, Eq)]
                    pub struct BlameEntry {
                    // ... existing code ...
                "#}
                .into(),
                expected_output_variants: vec![
                    include_str!("fixtures/delete_run_git_blame/after.rs").into(),
                ],
            },
        );
    }

    fn eval(iterations: usize, expected_pass_ratio: f32, eval: Eval) {
        let executor = gpui::background_executor();
        let (tx, rx) = mpsc::channel();
        for _ in 0..iterations {
            let eval = eval.clone();
            let tx = tx.clone();
            executor
                .spawn(async move {
                    let dispatcher = gpui::TestDispatcher::new(StdRng::from_entropy());
                    let mut cx = TestAppContext::build(dispatcher, None);
                    let output = cx.executor().block_test(async {
                        let test = agent_test(&mut cx).await;
                        apply_edits(
                            eval.input_path,
                            eval.input_content,
                            eval.instructions,
                            &test,
                            &mut cx,
                        )
                        .await
                    });
                    tx.send(output).unwrap();
                })
                .detach();
        }
        drop(tx);

        let mut finished_count = 0;
        report_progress(finished_count, iterations);

        let mut failed_count = 0;
        let mut failed_message = String::new();
        let mut failed_outputs = HashSet::default();
        while let Ok(output) = rx.recv() {
            if !eval.expected_output_variants.contains(&output) {
                failed_count += 1;
                if failed_outputs.insert(output.clone()) {
                    writeln!(
                        failed_message,
                        "=======\n{}\n=======",
                        pretty_assertions::StrComparison::new(
                            &output,
                            &eval.expected_output_variants[0]
                        )
                    )
                    .unwrap();
                }
            }

            finished_count += 1;
            report_progress(finished_count, iterations);
        }

        let actual_pass_ratio = (iterations - failed_count) as f32 / iterations as f32;
        assert!(
            actual_pass_ratio >= expected_pass_ratio,
            "Expected pass ratio: {}\nActual pass ratio: {}\nFailures: {}",
            expected_pass_ratio,
            actual_pass_ratio,
            failed_message
        );
    }

    fn report_progress(finished_count: usize, iterations: usize) {
        print!("\r\x1b[KFinished {}/{}", finished_count, iterations);
        std::io::stdout().flush().unwrap();
    }

    // #[gpui::test]
    // async fn test_extract_method(cx: &mut TestAppContext) {
    //     let EditAgentTest { fs, agent } = agent_test(cx).await;
    //     fs.insert_file(
    //         "/root/blame.rs",
    //         include_bytes!("../../git/src/blame.rs").into(),
    //     )
    //     .await;
    //     let path = agent
    //         .project
    //         .read_with(cx, |project, cx| {
    //             project.find_project_path("root/blame.rs", cx)
    //         })
    //         .unwrap();
    //     agent
    //         .interpret(
    //             path,
    //             indoc! {r#"
    //                 // ... existing code ...

    //                 async fn run_git_blame(
    //                     git_binary: &Path,
    //                     working_directory: &Path,
    //                     path: &Path,
    //                     contents: &Rope,
    //                 ) -> Result<String> {
    //                     let mut child = util::command::new_smol_command(git_binary)
    //                         .current_dir(working_directory)
    //                         .arg("blame")
    //                         .arg("--incremental")
    //                         .arg("--contents")
    //                         .arg("-")
    //                         .arg(path.as_os_str())
    //                         .stdin(Stdio::piped())
    //                         .stdout(Stdio::piped())
    //                         .stderr(Stdio::piped())
    //                         .spawn()
    //                         .map_err(|e| anyhow!("Failed to start git blame process: {}", e))?;

    //                     let stdin = child
    //                         .stdin
    //                         .as_mut()
    //                         .context("failed to get pipe to stdin of git blame command")?;

    //                     for chunk in contents.chunks() {
    //                         stdin.write_all(chunk.as_bytes()).await?;
    //                     }
    //                     stdin.flush().await?;

    //                     let output = child
    //                         .output()
    //                         .await
    //                         .map_err(|e| anyhow!("Failed to read git blame output: {}", e))?;

    //                     handle_git_blame_result(output)
    //                 }

    //                 fn handle_git_blame_result(output: std::process::Output) -> Result<String> {
    //                     if !output.status.success() {
    //                         let stderr = String::from_utf8_lossy(&output.stderr);
    //                         let trimmed = stderr.trim();
    //                         if trimmed == GIT_BLAME_NO_COMMIT_ERROR || trimmed.contains(GIT_BLAME_NO_PATH) {
    //                             return Ok(String::new());
    //                         }
    //                         return Err(anyhow!("git blame process failed: {}", stderr));
    //                     }

    //                     Ok(String::from_utf8(output.stdout)?)
    //                 }

    //                 // ... existing code ...
    //             "#}
    //             .into(),
    //             &mut cx.to_async(),
    //         )
    //         .await
    //         .unwrap();
    // }

    async fn apply_edits(
        path: impl AsRef<Path>,
        content: impl Into<Arc<str>>,
        instructions: impl Into<String>,
        test: &EditAgentTest,
        cx: &mut TestAppContext,
    ) -> String {
        let path = test
            .agent
            .project
            .read_with(cx, |project, cx| project.find_project_path(path, cx))
            .unwrap();
        let buffer = test
            .agent
            .project
            .update(cx, |project, cx| project.open_buffer(path, cx))
            .await
            .unwrap();
        buffer.update(cx, |buffer, cx| buffer.set_text(content, cx));
        let edits = test
            .agent
            .interpret(buffer.clone(), instructions.into(), &mut cx.to_async())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        buffer.update(cx, |buffer, cx| {
            buffer.edit(edits, None, cx);
            buffer.text()
        })
    }

    struct EditAgentTest {
        fs: Arc<FakeFs>,
        agent: EditAgent,
    }

    async fn agent_test(cx: &mut TestAppContext) -> EditAgentTest {
        cx.executor().allow_parking();
        cx.update(settings::init);
        cx.update(Project::init_settings);
        cx.update(language::init);
        cx.update(gpui_tokio::init);
        cx.update(client::init_settings);

        let fs = FakeFs::new(cx.executor().clone());
        fs.insert_tree("/root", json!({})).await;
        let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
        let model = cx
            .update(|cx| {
                let http_client = ReqwestClient::user_agent("agent tests").unwrap();
                cx.set_http_client(Arc::new(http_client));

                let client = Client::production(cx);
                let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
                language_model::init(client.clone(), cx);
                language_models::init(user_store.clone(), client.clone(), fs.clone(), cx);

                let models = LanguageModelRegistry::read_global(cx);
                let model = models
                    .available_models(cx)
                    .find(|model| model.id().0 == "gemini-2.0-flash")
                    .unwrap();

                let provider = models.provider(&model.provider_id()).unwrap();
                let authenticated = provider.authenticate(cx);

                cx.spawn(async move |_| {
                    authenticated.await.unwrap();
                    model
                })
            })
            .await;

        EditAgentTest {
            fs,
            agent: EditAgent::new(model, project, Templates::new()),
        }
    }
}
