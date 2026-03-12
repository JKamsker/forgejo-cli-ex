use crate::cli::{TokenCommand, TokenMintCommand};

pub async fn run(args: TokenCommand) -> eyre::Result<()> {
    match args {
        TokenCommand::Mint(command) => match command {
            TokenMintCommand::Nuget(args) => crate::auth::run_nuget_api_key(args).await,
        },
    }
}
