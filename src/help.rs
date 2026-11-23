use std::ffi::OsString;

use crate::provider::RelayError;

const OVERVIEW: &str = "\
ox connects Codex and Claude Code to your model API providers.

Get started
  Use a terminal client  Run ox setup, load your API key, then run ox doctor.
                         Start ox codex or ox claude from your project.
  Connect a desktop app  Run ox help desktop for Codex and Claude Desktop.
  Share across devices   Run ox help share for a home server and Tailscale.
  Use T3 Code            Run ox help t3 for Codex and Claude Agent SDK setup.

Commands
  ox                         Open setup on first use, or launch Codex once configured
  ox setup [KEY] [options]   Add a provider with prompts or flags
  ox codex [args...]         Launch Codex with the gateway
  ox claude [args...]        Launch Claude Code with the gateway
  ox provider add [KEY]      Add another provider
  ox doctor                  Check setup offline
  ox doctor --live --provider KEY
                             Test one live API request
  ox serve [--port PORT]     Run a persistent loopback gateway
  ox help [setup|desktop|share|t3]
                             Show help for a task
  ox --version               Print the installed ox version
  ox --help                  Show this help

Run ox setup --help for options and copyable examples.
Use plain codex or claude for your usual sign-in.";

const DESKTOP: &str = "\
Connect a desktop app to ox

1. On the machine that will host ox, run ox setup and ox doctor.
   Load the upstream API keys there. Run ox help share to create a separate
   OX_GATEWAY_KEY and start ox serve. Use a Tailscale HTTPS URL for another device.
2. On the client, use the same gateway key and a model alias from /v1/models.

Codex Desktop
  If your app allows adding gateways, open Settings > Model Providers & Gateways
  and choose Add > Gateway or custom provider. Name it ox. Set the base URL to
  your gateway URL with /v1 appended. Set the API key variable to OX_GATEWAY_KEY.
  Load that variable into the app process. A terminal export reaches programs
  launched from that terminal.
  For manual setup, merge the example below into your user config.toml
  (normally ~/.codex/config.toml). It uses /v1/catalog for model discovery.

Claude Desktop on 3P
  Open Configure Third-Party Inference. Choose Gateway with Bearer auth.
  Use the HTTPS gateway origin with no /v1 suffix. Enter the gateway key.
  Start with a native Anthropic route and use its exact ox model alias.

Try it
  Start a fresh local task using ox. Ask it to read a small file in a test
  project. A streamed reply with a detail from the file checks both inference
  and local file tools.
  The gateway is tested with stock Codex App Server and Claude Code CLI.
  Desktop UI flows still need a smoke test on an installed app.

See docs/shared-gateway.md in the ox repository for the client examples.";

const SHARE: &str = r#"Share one ox gateway across devices

1. On the host, add a provider with ox setup, load its API key, and run ox doctor.
2. Generate a gateway key once with openssl rand -hex 32 and save it in your
   password manager. Load the same key as OX_GATEWAY_KEY on each restart.
   For Bash or Zsh, paste it at this hidden prompt:

     printf 'Gateway key (input hidden): '
     IFS= read -rs OX_GATEWAY_KEY
     printf '\n'
     export OX_GATEWAY_KEY
     ox serve --port 8787

   It listens on http://127.0.0.1:8787. Keep this terminal open, or use a service
   manager for an always-on host.
3. To connect another device on your tailnet, enable Tailscale HTTPS and run
   this on the host in a second terminal:

     tailscale serve --bg http://127.0.0.1:8787
     tailscale serve status

   Copy the HTTPS URL from Serve. Restrict access with your tailnet policy.
   The gateway key grants access to all configured models on this host.
4. From a client device, load the same OX_GATEWAY_KEY and replace the example URL:

     printf 'header = "Authorization: Bearer %s"\n' "$OX_GATEWAY_KEY" |
       curl --fail --silent --show-error --config - 'https://ox.example-tailnet.ts.net/v1/models'

   A list of model aliases confirms the connection. HTTP 401 means the client
   key is missing or differs from the one loaded by the host.
5. Connect your client. Use the gateway URL plus /v1 for Codex, and the origin
   for Claude. Clients use the gateway key. Upstream API keys stay on the host.
   Run ox help desktop for desktop fields. For stock Claude Code, set
   ANTHROPIC_BASE_URL to the origin and ANTHROPIC_AUTH_TOKEN to the gateway key.

See docs/shared-gateway.md in the ox repository for the Linux service example
and T3 Code setup."#;

const T3: &str = "\
Connect T3 Code to ox

Run ox serve on the machine that holds the API keys. See ox help share if the
T3 execution host is on another device.

In T3 Code, open Settings > Providers and add an instance named ox.

Codex
  Use the stock Codex provider with a custom gateway. Use the gateway URL
  ending in /v1 and its key. Add the ox model alias as a custom model.

Claude
  Use the Claude provider, which runs the Claude Agent SDK. Keep its usual
  runtime. Under Environment variables, set:
    ANTHROPIC_BASE_URL    Gateway origin, without /v1
    ANTHROPIC_AUTH_TOKEN  Gateway key (mark it sensitive)
    ANTHROPIC_API_KEY     Explicitly empty
  Add the ox model alias as a custom model. Use a separate Claude config
  directory if you also use Claude sign-in on this execution host.

Enable the instance for your project. Start a new thread with that instance
and the full provider/model-id alias.
See docs/shared-gateway.md in the ox repository for the client examples.";

pub(crate) fn overview() {
    println!("{OVERVIEW}");
}

pub(crate) fn dispatch(args: &[OsString]) -> Option<Result<u8, RelayError>> {
    match args {
        [flag, ..] if flag == "-h" || flag == "--help" => overview(),
        [command] if command == "help" => overview(),
        [command, topic] if command == "help" => match topic.to_str() {
            Some("setup") => return Some(crate::setup::print_setup_help()),
            Some("desktop") => println!(
                "{DESKTOP}\n\nCodex manual configuration\n\
                 Replace the example URL and model alias before merging:\n\n{}",
                include_str!("../examples/codex-gateway.toml").trim_end()
            ),
            Some("share") => println!("{SHARE}"),
            Some("t3") => println!("{T3}"),
            _ => return Some(Err("usage: ox help [setup|desktop|share|t3]".into())),
        },
        [command, ..] if command == "help" => {
            return Some(Err("usage: ox help [setup|desktop|share|t3]".into()));
        }
        [command, flag] if flag == "-h" || flag == "--help" => match command.to_str() {
            Some("setup") => return Some(crate::setup::print_setup_help()),
            Some(client @ ("codex" | "claude")) => println!(
                "Usage: ox {client} [client arguments]\n\
                 Run ox setup to add a provider, then launch from your project.\n\
                 Example: ox {client} --model provider/model-id\n\
                 Run {client} --help for arguments accepted by the stock client."
            ),
            _ => return None,
        },
        _ => return None,
    }
    Some(Ok(0))
}
