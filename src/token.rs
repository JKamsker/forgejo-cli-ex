use crate::cli::{TokenCommand, TokenListCommand, TokenMintCommand};

pub async fn run(args: TokenCommand) -> eyre::Result<()> {
    match args {
        TokenCommand::Mint(command) => match command {
            TokenMintCommand::Nuget(args) => crate::auth::run_nuget_api_key(args).await,
        },
        TokenCommand::List(args) => run_list(args).await,
    }
}

async fn run_list(args: TokenListCommand) -> eyre::Result<()> {
    let target = crate::target::resolve_target(
        args.target.host.as_deref(),
        args.target.repo.as_ref(),
        args.target.remote.as_deref(),
    )?;

    let token = crate::store::get_fj_api_token_for_base_url(&target.base_url)?
        .ok_or_else(|| crate::actions::fj_missing_api_token_error(&target.base_url))?;
    let client = crate::api::ApiClient::new(&target.base_url, &token)?;

    let me: crate::api::AuthenticatedUser = client.get_json(&client.api_v1_url("/user")).await?;
    let url = client.api_v1_url(&format!(
        "/users/{}/tokens?page={}&limit={}",
        urlencoding::encode(&me.login),
        args.page,
        args.limit
    ));
    let tokens: Vec<crate::api::ListedAccessToken> = client.get_json(&url).await?;

    if args.json {
        let payload = serde_json::json!({
            "baseUrl": target.base_url,
            "username": me.login,
            "page": args.page,
            "limit": args.limit,
            "tokens": tokens,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let show_header = crate::output::should_print_header(args.header, args.no_header);
    let headers = vec!["Id", "Name", "LastEight", "Scopes"];
    let rows = tokens
        .into_iter()
        .map(|tok| {
            vec![
                tok.id.to_string(),
                tok.name,
                tok.token_last_eight,
                tok.scopes.join(","),
            ]
        })
        .collect::<Vec<_>>();
    crate::output::print_table(&headers, &rows, show_header);
    Ok(())
}
