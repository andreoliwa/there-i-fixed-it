use std::{
    fmt::Display,
    process::{Output, Stdio},
    sync::Arc,
};

use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::{
    eyre::{eyre, Context},
    Help, Result, SectionExt,
};
use tokio::{fs, process::Command};
use tracing::{debug, info, instrument, trace};

use crate::Repository;

use super::{glob_pattern::GlobPattern, FileOperation, Plan};

pub struct PlanExecutor {
    plan: Arc<Plan>,
    repository: Repository,
    directory: Utf8PathBuf,
}

impl PlanExecutor {
    pub fn new(plan: Arc<Plan>, repository: Repository, repositories_folder: &Utf8Path) -> Self {
        let directory = repositories_folder.join("repos").join(&repository.name);

        Self {
            plan,
            repository,
            directory,
        }
    }
    #[instrument(skip(self), fields(repository_name = self.repository.name.as_str()))]
    pub async fn process(&self) -> Result<()> {
        debug!("started");

        self.clone_repository(&self.directory).await?;
        self.ensure_branch(&self.directory).await?;

        if !self.process_operations().await? {
            return Ok(());
        }

        self.commit(&self.directory).await?;
        self.push(&self.directory).await?;
        self.open_pr().await?;
        Ok(())
    }

    #[instrument(skip(self))]
    async fn clone_repository(&self, path: &Utf8Path) -> Result<()> {
        if path.exists() {
            debug!("Skipping");
            return Ok(());
        }

        let output = Command::new("git")
            .args(&["clone", &self.repository.ssh_url.as_str()])
            .arg(path)
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .stdin(Stdio::null())
            .spawn()?
            .wait_with_output()
            .await?;
        self.check_process(&output)
            .wrap_err("failed to clone repository")?;
        info!("done");
        Ok(())
    }

    fn check_process(&self, output: &Output) -> Result<String> {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            return Ok(stdout.to_string());
        }

        let err = eyre!("failed to run command")
            .with_section(move || format!("Exit code: {:?}", output.status.code()))
            .with_section(move || stdout.trim().to_string().header("Stdout:"))
            .with_section(move || stderr.trim().to_string().header("Stderr:"));

        Err(err)
    }

    #[instrument(skip(self))]
    async fn ensure_branch(&self, directory: &Utf8Path) -> Result<()> {
        let output = self
            .git_output(directory, &["branch", "--show-current"])
            .await
            .wrap_err("failed to list branch")?;
        let output = output.trim();
        if output == self.plan.branch_name {
            debug!("branch already checked out");
            return Ok(());
        }

        self.git_output(directory, &["reset", "--hard"])
            .await
            .wrap_err("failed to reset branch")?;
        self.git_output(directory, &["checkout", &self.repository.default_branch])
            .await
            .wrap_err("failed to checkout default branch")?;

        self.git_output(directory, &["pull", "-r"])
            .await
            .wrap_err("failed to pull changes")?;

        let _ = self
            .git_output(
                directory,
                &["checkout", "-b", self.plan.branch_name.as_str()],
            )
            .await
            .wrap_err("failed to checkout new branch")?;
        debug!("changed to branch {}", self.plan.branch_name);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn git_output(&self, directory: &Utf8Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .stdin(Stdio::null())
            .current_dir(&self.directory)
            .spawn()?
            .wait_with_output()
            .await?;
        Ok(self.check_process(&output)?)
    }

    async fn process_operations(&self) -> Result<bool> {
        let mut files_changed = false;
        for operation in &self.plan.file_operations {
            files_changed |= self.process_operation(operation).await?;
        }
        Ok(files_changed)
    }

    async fn process_operation(&self, operation: &FileOperation) -> Result<bool> {
        let files = self.list_files(&self.directory, &operation.pattern).await?;
        let files = files.iter().map(|f| f.as_path()).collect::<Vec<_>>();

        self.process_files(&files, operation).await
    }

    #[instrument(skip(self))]
    async fn list_files(
        &self,
        directory: &Utf8Path,
        pattern: &GlobPattern,
    ) -> Result<Vec<Utf8PathBuf>> {
        let mut output = vec![];
        let glob_pattern = directory.join(pattern.as_str());

        for entry in glob::glob(&glob_pattern.as_str())? {
            let entry = entry?;
            if !entry.is_file() {
                continue;
            }
            output.push(Utf8PathBuf::from_path_buf(entry).unwrap());
        }

        Ok(output)
    }

    #[instrument(skip(self, files))]
    async fn process_files(&self, files: &[&Utf8Path], operation: &FileOperation) -> Result<bool> {
        let mut files_changed = false;
        for file in files {
            files_changed |= self.process_file(file, operation).await?;
        }
        Ok(files_changed)
    }

    #[instrument(skip(self, operation))]
    async fn process_file(&self, file: &Utf8Path, operation: &FileOperation) -> Result<bool> {
        trace!("fixing file");
        let old_text = fs::read_to_string(file).await?;
        // TODO: After https://github.com/rust-lang/rust/issues/65143
        // is merged, would Cow<T>.is_owned() enough to find out if the file changed?
        let mut new_text = old_text.clone();
        for processor in &operation.processors {
            // TODO: Find a way to make this CoW
            new_text = processor.process(&new_text).to_string();
        }

        if old_text == new_text {
            return Ok(false);
        }

        fs::write(file, &new_text).await?;

        trace!("done");
        Ok(true)
    }

    #[instrument(skip(self, directory))]
    async fn commit(&self, directory: &Utf8Path) -> Result<()> {
        debug!("commiting");
        let last_commit = self
            .git_output(directory, &["log", "--format=%B", "-n", "1"])
            .await?;
        if last_commit.starts_with(&format!("{}\n", &self.plan.git_message)) {
            debug!("commit already done");
            return Ok(());
        }
        self.git_output(directory, &["commit", "-a", "-m", &self.plan.git_message])
            .await
            .wrap_err("failed to commit changes")?;
        Ok(())
    }

    #[instrument(skip(self, directory))]
    async fn push(&self, directory: &Utf8Path) -> Result<()> {
        debug!("pushing");
        let output = self
            .git_output(
                directory,
                &["push", "-u", "-f", "origin", &self.plan.branch_name],
            )
            .await
            .wrap_err("failed to push changes")?;
        trace!("git: {:?}", output);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn open_pr(&self) -> Result<()> {
        if self
            .plan
            .get_provider()
            .is_pr_open(&self.repository.name, &self.plan.branch_name)
            .await?
        {
            info!("pr already opened");
            return Ok(());
        }

        let body = self.plan.pull_request_body.as_ref().map(|b| b.as_str());
        let title = self
            .plan
            .pull_request_title
            .as_ref()
            .unwrap_or(&&self.plan.git_message);

        self.plan
            .get_provider()
            .open_pr(
                &self.repository.name,
                &self.repository.default_branch,
                &self.plan.branch_name,
                title.as_str(),
                body,
            )
            .await?;
        info!("done");
        Ok(())
    }
}

impl Display for PlanExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.repository.name)
    }
}

#[cfg(test)]
mod tests {
    use std::{process::Stdio, sync::Arc};

    use camino::{Utf8Path, Utf8PathBuf};
    use tempdir::TempDir;
    use tokio::{io::AsyncWriteExt, process::Command};

    use crate::{plan::plan_from_file, Repository};

    use super::PlanExecutor;

    const CREATE_REPOSITORY_SCRIPT: &str = r#"
    set -ex
    cd $1
    mkdir destination.git
    cd destination.git
    git init -b main --bare
    cd ..
    git clone destination.git setup
    cd setup
    echo "enabled = True" > file.py
    git add .
    git commit -m"Initial commit"
    git push -u origin main
    "#;

    #[tokio::test]
    async fn test_executor_flow() {
        crate::setup_error_handlers().ok();
        let plan_file = Utf8PathBuf::from("tests/fixtures/simple-plan.toml");
        let plan = Arc::new(plan_from_file(&plan_file).await.unwrap());

        let repositories = plan.get_provider().list_repositories(false).await.unwrap();
        assert_eq!(repositories.len(), 1);

        for repository in repositories {
            let (repository, temp) = create_fake_repository(repository).await;
            let path = Utf8Path::from_path(temp.path()).unwrap();
            let executor = PlanExecutor::new(plan.clone(), repository, path);
            executor.process().await.unwrap();
        }
    }

    async fn create_fake_repository(repository: Repository) -> (Repository, TempDir) {
        let temp = TempDir::new("fake-repository").unwrap();

        let mut command = Command::new("sh")
            .arg("-s")
            .arg(&temp.path())
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();

        let mut stdin = command.stdin.take().unwrap();
        stdin
            .write_all(CREATE_REPOSITORY_SCRIPT.as_bytes())
            .await
            .unwrap();

        drop(stdin);

        assert_eq!(command.wait().await.unwrap().code(), Some(0));

        let new_repository = Repository {
            ssh_url: temp
                .path()
                .join("destination.git")
                .to_string_lossy()
                .to_string(),
            ..repository
        };

        (new_repository, temp)
    }
}