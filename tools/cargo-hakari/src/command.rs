// Copyright (c) The cargo-guppy Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::{cargo_cli::CargoCli, output::OutputOpts};
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre::{bail, Result, WrapErr};
use colored::Colorize;
use guppy::{
    graph::{PackageGraph, PackageSet},
    MetadataCommand,
};
use hakari::{
    cli_ops::{HakariInit, WorkspaceOps},
    diffy::PatchFormatter,
    summaries::HakariConfig,
    HakariBuilder, HakariCargoToml, HakariOutputOptions,
};
use log::{error, info};
use std::convert::TryFrom;
use structopt::{clap::AppSettings, StructOpt};

/// The location of the configuration used by `cargo hakari`, relative to the workspace root.
pub static CONFIG_PATH: &str = ".guppy/hakari.toml";

/// The comment to add to the top of the config file.
pub static CONFIG_COMMENT: &str = r#"# This file contains settings for `cargo hakari`.
# See https://docs.rs/cargo-hakari/*/cargo_hakari/config for a full list of options.
"#;

/// The comment to add to the top of the workspace-hack package's Cargo.toml.
pub static CARGO_TOML_COMMENT: &str = r#"# This file is generated by `cargo hakari`.
# To regenerate, run:
#     cargo hakari generate
"#;

/// The message to write into a disabled Cargo.toml.
pub static DISABLE_MESSAGE: &str = r#"
# Disabled by running `cargo hakari disable`.
# To re-enable, run:
#     cargo hakari generate
"#;

/// Set up and manage workspace-hack crates.
///
/// For more about cargo-hakari, see <https://docs.rs/cargo-hakari>.
#[derive(Debug, StructOpt)]
pub struct Args {
    #[structopt(flatten)]
    global: GlobalOpts,
    #[structopt(subcommand)]
    command: Command,
}

impl Args {
    /// Executes the command.
    ///
    /// Returns the exit status, or an error on failure.
    pub fn exec(self) -> Result<i32> {
        self.command.exec(self.global.output)
    }
}

#[derive(Clone, Debug, StructOpt)]
struct GlobalOpts {
    #[structopt(flatten)]
    output: OutputOpts,
}

/// Manage workspace-hack crates.
#[derive(Debug, StructOpt)]
enum Command {
    /// Initialize a workspace-hack crate and a hakari.toml file
    #[structopt(name = "init")]
    Initialize {
        /// Path to generate the workspace-hack crate at, relative to the current directory.
        path: Utf8PathBuf,

        /// The name of the crate (default: derived from path)
        #[structopt(long, short)]
        package_name: Option<String>,

        /// Skip writing a stub config to hakari.toml
        #[structopt(long)]
        skip_config: bool,

        /// Print operations that need to be performed, but do not actually perform them.
        ///
        /// Exits with status 1 if any operations need to be performed. Can be combined with
        /// `--quiet`.
        #[structopt(long, short = "n", conflicts_with = "yes")]
        dry_run: bool,

        /// Proceed with the operation without prompting for confirmation.
        #[structopt(long, short, conflicts_with = "dry-run")]
        yes: bool,
    },

    #[structopt(flatten)]
    WithBuilder(CommandWithBuilder),
}

impl Command {
    fn exec(self, output: OutputOpts) -> Result<i32> {
        output.init_logger();
        let metadata_command = MetadataCommand::new();
        let package_graph = metadata_command
            .build_graph()
            .context("building package graph failed")?;
        let config_path = config_path(&package_graph);

        match self {
            Command::Initialize {
                path,
                package_name,
                skip_config,
                dry_run,
                yes,
            } => {
                let package_name = match package_name.as_deref() {
                    Some(name) => name,
                    None => match path.file_name() {
                        Some(name) => name,
                        None => bail!("invalid path {}", path),
                    },
                };

                let workspace_path =
                    cwd_rel_to_workspace_rel(&path, package_graph.workspace().root())?;

                let mut init = HakariInit::new(&package_graph, package_name, &workspace_path)
                    .with_context(|| "error initializing Hakari package")?;
                init.set_cargo_toml_comment(CARGO_TOML_COMMENT);
                if !skip_config {
                    init.set_config(CONFIG_PATH.as_ref(), CONFIG_COMMENT)
                        .with_context(|| "error initializing Hakari package")?;
                }

                let ops = init.make_ops();
                apply_on_dialog(dry_run, yes, &ops, &output, || {
                    let steps = [
                        format!("* configure at {}", CONFIG_PATH.bold()),
                        format!(
                            "* run {} to generate contents",
                            "cargo hakari generate".bold()
                        ),
                        format!(
                            "* run {} to add dependency lines",
                            "cargo hakari manage-deps".bold()
                        ),
                    ];
                    info!("next steps:\n{}\n", steps.join("\n"));
                    Ok(())
                })
            }
            Command::WithBuilder(cmd) => {
                let config = read_config(&config_path)?;
                let builder = config
                    .builder
                    .to_hakari_builder(&package_graph)
                    .with_context(|| {
                        format!("could not resolve Hakari config at {}", &config_path)
                    })?;
                let hakari_output = config.output.to_options();
                cmd.exec(builder, hakari_output, output)
            }
        }
    }
}

#[derive(Debug, StructOpt)]
enum CommandWithBuilder {
    /// Generate or update the contents of the workspace-hack crate
    Generate {
        /// Print a diff of contents instead of writing them out. Can be combined with `--quiet`.
        ///
        /// Exits with status 1 if the contents are different.
        #[structopt(long)]
        diff: bool,
    },

    /// Perform verification of the workspace-hack crate
    ///
    /// Check that the workspace-hack crate succeeds at its goal of building one version of
    /// every non-omitted third-party crate.
    ///
    /// Exits with status 1 if verification failed.
    Verify,

    /// Manage dependencies from workspace crates to workspace-hack.
    ///
    /// * Add the dependency to all non-excluded workspace crates.
    /// * Remove the dependency from all excluded workspace crates.
    ManageDeps {
        #[structopt(flatten)]
        packages: PackageSelection,

        /// Print operations that need to be performed, but do not actually perform them.
        ///
        /// Exits with status 1 if any operations need to be performed. Can be combined with
        /// `--quiet`.
        #[structopt(long, short = "n", conflicts_with = "yes")]
        dry_run: bool,

        /// Proceed with the operation without prompting for confirmation.
        #[structopt(long, short, conflicts_with = "dry-run")]
        yes: bool,
    },

    /// Remove dependencies from workspace crates to workspace-hack.
    RemoveDeps {
        #[structopt(flatten)]
        packages: PackageSelection,

        /// Print operations that need to be performed, but do not actually perform them.
        ///
        /// Exits with status 1 if any operations need to be performed. Can be combined with
        /// `--quiet`.
        #[structopt(long, short = "n", conflicts_with = "yes")]
        dry_run: bool,

        /// Proceed with the operation without prompting for confirmation.
        #[structopt(long, short, conflicts_with = "dry-run")]
        yes: bool,
    },

    /// Publish a package after removing the workspace-hack dependency from it.
    ///
    /// When publishing a crate containing a workspace-hack dependency, it needs to be removed
    /// before it is published. This command automates that process, adding the
    /// workspace-hack dependency back again after publishing.
    ///
    /// Trailing arguments are passed through to cargo publish.
    #[structopt(setting = AppSettings::TrailingVarArg, setting = AppSettings::AllowLeadingHyphen)]
    Publish {
        /// The name of the package to publish.
        #[structopt(long, short)]
        package: String,

        /// Arguments to pass through to `cargo publish`.
        #[structopt(multiple = true)]
        pass_through: Vec<String>,
    },

    /// Disables the workspace-hack crate
    ///
    /// Removes all the generated contents from the workspace-hack crate.
    Disable {
        /// Print a diff of changes instead of writing them out. Can be combined with `--quiet`.
        ///
        /// Exits with status 1 if the contents are different.
        #[structopt(long)]
        diff: bool,
    },
}

impl CommandWithBuilder {
    fn exec(
        self,
        builder: HakariBuilder<'_>,
        hakari_output: HakariOutputOptions,
        output: OutputOpts,
    ) -> Result<i32> {
        let hakari_package = *builder
            .hakari_package()
            .expect("hakari-package must be specified in hakari.toml");

        match self {
            CommandWithBuilder::Generate { diff } => {
                let hakari = builder.compute();
                let toml_out = hakari
                    .to_toml_string(&hakari_output)
                    .with_context(|| "error generating new hakari.toml")?;

                let existing_toml = hakari
                    .read_toml()
                    .expect("hakari-package must be specified")?;

                write_to_cargo_toml(existing_toml, &toml_out, diff, output)
            }
            CommandWithBuilder::Verify => match builder.verify() {
                Ok(()) => {
                    info!(
                        "workspace-hack package {} works correctly",
                        hakari_package.name().bold()
                    );
                    Ok(0)
                }
                Err(errs) => {
                    info!(
                        "workspace-hack package {} didn't work correctly:\n{}",
                        hakari_package.name().bold(),
                        errs
                    );
                    Ok(1)
                }
            },
            CommandWithBuilder::ManageDeps {
                packages,
                dry_run,
                yes,
            } => {
                let ops = builder
                    .manage_dep_ops(&packages.to_package_set(builder.graph())?)
                    .expect("hakari-package must be specified in hakari.toml");
                if ops.is_empty() {
                    info!("no operations to perform");
                    return Ok(0);
                }

                apply_on_dialog(dry_run, yes, &ops, &output, || regenerate_lockfile(output))
            }
            CommandWithBuilder::RemoveDeps {
                packages,
                dry_run,
                yes,
            } => {
                let ops = builder
                    .remove_dep_ops(&packages.to_package_set(builder.graph())?, false)
                    .expect("hakari-package must be specified in hakari.toml");
                if ops.is_empty() {
                    info!("no operations to perform");
                    return Ok(0);
                }

                apply_on_dialog(dry_run, yes, &ops, &output, || regenerate_lockfile(output))
            }
            CommandWithBuilder::Publish {
                package,
                pass_through,
            } => {
                let workspace = builder.graph().workspace();
                let package = workspace.member_by_name(&package)?;
                let package_set = package.to_package_set();
                let remove_ops = builder
                    .remove_dep_ops(&package_set, false)
                    .expect("hakari-package must be specified in hakari.toml");
                let add_later = if remove_ops.is_empty() {
                    info!(
                        "dependency from {} to {} not present",
                        package.name().bold(),
                        hakari_package.name().bold()
                    );
                    false
                } else {
                    info!(
                        "removing dependency from {} to {}",
                        package.name().bold(),
                        hakari_package.name().bold()
                    );
                    remove_ops.apply().wrap_err_with(|| {
                        format!("error removing dependency from {}", package.name())
                    })?;
                    true
                };

                let mut cargo_cli = CargoCli::new("publish", output);
                cargo_cli.add_args(pass_through.iter().map(|arg| arg.as_str()));
                // Also set --allow-dirty because we make some changes to the working directory.
                // TODO: is there a better way to handle this?
                cargo_cli.add_arg("--allow-dirty");

                let workspace_dir = package
                    .source()
                    .workspace_path()
                    .expect("package is in workspace");
                let abs_path = workspace.root().join(workspace_dir);

                let all_args = cargo_cli.all_args().join(" ");

                info!("{} {}\n---", "executing".bold(), all_args);
                let expression = cargo_cli.to_expression().dir(&abs_path);

                // The current PackageGraph doesn't know about the changes to the workspace yet, so
                // force an add.
                let add_ops = builder
                    .add_dep_ops(&package_set, true)
                    .expect("hakari-package must be specified in hakari.toml");

                match (expression.run(), add_later) {
                    (Ok(_), true) => {
                        // Execution was successful + need to add the dep back.
                        info!(
                            "re-adding dependency from {} to {}",
                            package.name().bold(),
                            hakari_package.name().bold()
                        );
                        add_ops.apply()?;
                        regenerate_lockfile(output)?;
                        Ok(0)
                    }
                    (Ok(_), false) => {
                        // Execution was successful but no need to add the dep back.
                        Ok(0)
                    }
                    (Err(err), true) => {
                        // Execution failed + need to add the dep back.
                        eprintln!("---");
                        error!("execution failed, rolling back changes");
                        add_ops.apply()?;
                        regenerate_lockfile(output)?;
                        Err(err).wrap_err_with(|| format!("`{}` failed", all_args))
                    }
                    (Err(err), false) => {
                        // Execution failed, no need to add the dep back.
                        Err(err).wrap_err_with(|| format!("`{}` failed", all_args))
                    }
                }
            }
            CommandWithBuilder::Disable { diff } => {
                let existing_toml = builder
                    .read_toml()
                    .expect("hakari-package must be specified")?;
                write_to_cargo_toml(existing_toml, DISABLE_MESSAGE, diff, output)
            }
        }
    }
}

/// Support for packages and features.
#[derive(Debug, StructOpt)]
struct PackageSelection {
    #[structopt(long = "package", short, number_of_values = 1)]
    /// Packages to operate on (default: entire workspace)
    packages: Vec<String>,
}

impl PackageSelection {
    /// Converts this selection into a `PackageSet`.
    fn to_package_set<'g>(&self, graph: &'g PackageGraph) -> Result<PackageSet<'g>> {
        if !self.packages.is_empty() {
            Ok(graph.resolve_workspace_names(&self.packages)?)
        } else {
            Ok(graph.resolve_workspace())
        }
    }
}

// ---
// Helper methods
// ---

fn cwd_rel_to_workspace_rel(path: &Utf8Path, workspace_root: &Utf8Path) -> Result<Utf8PathBuf> {
    let abs_path = if path.is_absolute() {
        path.to_owned()
    } else {
        let cwd = std::env::current_dir().with_context(|| "could not access current dir")?;
        let mut cwd = Utf8PathBuf::try_from(cwd).with_context(|| "current dir is invalid UTF-8")?;
        cwd.push(path);
        cwd
    };

    abs_path
        .strip_prefix(workspace_root)
        .map(|p| p.to_owned())
        .with_context(|| {
            format!(
                "path {} is not inside workspace root {}",
                abs_path, workspace_root
            )
        })
}

fn config_path(package_graph: &PackageGraph) -> Utf8PathBuf {
    package_graph.workspace().root().join(CONFIG_PATH)
}

fn read_config(path: &Utf8Path) -> Result<HakariConfig> {
    let config = std::fs::read_to_string(path)
        .with_context(|| format!("could not read hakari config at {}", path))?;
    config
        .parse()
        .with_context(|| format!("could not deserialize hakari config at {}", path))
}

fn write_to_cargo_toml(
    existing_toml: HakariCargoToml,
    new_contents: &str,
    diff: bool,
    output: OutputOpts,
) -> Result<i32> {
    if diff {
        let patch = existing_toml.diff_toml(new_contents);
        let mut formatter = PatchFormatter::new();
        if output.should_colorize() {
            formatter = formatter.with_color();
        }
        info!("\n{}", formatter.fmt_patch(&patch));
        if patch.hunks().is_empty() {
            // No differences.
            Ok(0)
        } else {
            Ok(1)
        }
    } else {
        if !existing_toml.is_changed(new_contents) {
            info!("no changes detected");
        } else {
            existing_toml
                .write_to_file(new_contents)
                .with_context(|| "error writing updated Hakari contents")?;
            info!("contents updated");
            regenerate_lockfile(output)?;
        }
        Ok(0)
    }
}

fn apply_on_dialog(
    dry_run: bool,
    yes: bool,
    ops: &WorkspaceOps<'_, '_>,
    output: &OutputOpts,
    after: impl FnOnce() -> Result<()>,
) -> Result<i32> {
    let mut display = ops.display();
    if output.should_colorize() {
        display.color();
    }
    info!("operations to perform:\n\n{}", display);

    if dry_run {
        // dry-run + non-empty ops implies exit status 1.
        return Ok(1);
    }

    let should_apply = if yes {
        true
    } else {
        let colorful_theme = dialoguer::theme::ColorfulTheme::default();
        let mut confirm = if output.should_colorize() {
            dialoguer::Confirm::with_theme(&colorful_theme)
        } else {
            dialoguer::Confirm::with_theme(&dialoguer::theme::SimpleTheme)
        };
        confirm
            .with_prompt("proceed?")
            .default(true)
            .show_default(true)
            .interact()
            .with_context(|| "error reading input")?
    };

    if should_apply {
        ops.apply()?;
        after()?;
        Ok(0)
    } else {
        Ok(1)
    }
}

/// Regenerate the lockfile after dependency updates.
fn regenerate_lockfile(output: OutputOpts) -> Result<()> {
    // This seems to be the cheapest way to update the lockfile.
    // cargo update -p <hakari-package> can sometimes cause unnecessary index updates.
    let cargo_cli = CargoCli::new("tree", output);
    cargo_cli
        .to_expression()
        .stdout_null()
        .run()
        .wrap_err("updating Cargo.lock failed")?;
    Ok(())
}
