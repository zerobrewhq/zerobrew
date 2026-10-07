use clap::Parser;
use console::style;
use zb_cli::{
    cli::{Cli, Commands},
    commands,
    init::ensure_init,
    logging,
    ui::Ui,
    utils::{get_prefix_path, get_root_path},
};
use zb_io::create_installer;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    logging::init(cli.verbose, cli.quiet);

    if let Err(e) = run(cli).await {
        eprintln!("{} {}", style("error:").red().bold(), e);
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), zb_core::Error> {
    let mut ui = Ui::new();

    if let Commands::Completion { shell } = cli.command {
        return commands::completion::execute(shell);
    }

    let root = get_root_path(cli.root);
    let prefix = get_prefix_path(cli.prefix, &root);

    if let Commands::Init { no_modify_path } = cli.command {
        return commands::init::execute(&root, &prefix, no_modify_path, &mut ui);
    }

    if !matches!(cli.command, Commands::Reset { .. }) {
        ensure_init(&root, &prefix, cli.auto_init, &mut ui)?;
    }

    let mut installer = create_installer(&root, &prefix, cli.concurrency)?;

    match cli.command {
        Commands::Init { .. } => unreachable!(),
        Commands::Completion { .. } => unreachable!(),
        Commands::Install {
            formulas,
            no_link,
            build_from_source,
        } => {
            commands::install::execute(
                &mut installer,
                formulas,
                no_link,
                build_from_source,
                &mut ui,
            )
            .await
        }
        Commands::Bundle { command } => {
            commands::bundle::execute(&mut installer, command, &mut ui).await
        }
        Commands::Uninstall { formulas, all } => {
            commands::uninstall::execute(&mut installer, formulas, all, &mut ui)
        }
        Commands::Migrate { yes, force } => {
            commands::migrate::execute(&mut installer, yes, force, &mut ui).await
        }
        Commands::Doctor { repair } => commands::doctor::execute(&mut installer, repair, &mut ui),
        Commands::List => commands::list::execute(&mut installer),
        Commands::Info { formula } => commands::info::execute(&mut installer, formula),
        Commands::Gc => commands::gc::execute(&mut installer),
        Commands::Update => commands::update::execute(&mut installer).await,
        Commands::Outdated { json } => {
            commands::outdated::execute(&mut installer, cli.quiet, cli.verbose > 0, json).await
        }
        Commands::Upgrade {
            formulas,
            build_from_source,
            no_link,
        } => {
            commands::upgrade::execute(
                &mut installer,
                formulas,
                build_from_source,
                no_link,
                &mut ui,
            )
            .await
        }
        Commands::Reset { yes } => commands::reset::execute(&root, &prefix, yes, &mut ui),
        Commands::Run { formula, args } => {
            commands::run::execute(&mut installer, formula, args).await
        }
    }
}
