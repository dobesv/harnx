//! Tool names agents and config refer to, checked against the names harnx
//! knows offline and the tools each configured server lists.

use std::collections::{BTreeMap, HashMap};

use globset::GlobSet;

use super::known_tools::{listed_registration, registration, visible_names, KnownNames};
use super::references::{command_missing, tool_server_file};
use super::tool_listing::{configured_names, list_servers, server_key, ServerListing};
use super::tool_refs::{collect_tool_refs, reach, ToolRef};
use super::{CheckContext, Code, ConfigLayout, Finding, LoadedAgent, Report};
use crate::config::{selector_could_match_server, Config, ToolServerConfig};
use crate::server_identity::ServerIdentity;
use crate::tool_selector::is_glob;

/// Largest edit distance a "did you mean" suggestion may be from the name.
const MAX_SUGGESTION_DISTANCE: usize = 2;

/// The pass: collect every reference, list every enabled server once if any
/// reference needs one, and check each reference; with `--tools skip`, run
/// nothing and only count what would need a server. Needs the loaded config,
/// so it does nothing when the config doesn't load.
pub async fn check_tools(
    context: &CheckContext<'_>,
    config: Option<&Config>,
    agents: &[LoadedAgent],
    ignore: &GlobSet,
    report: &mut Report,
) {
    let Some(config) = config else {
        return;
    };
    let refs = collect_tool_refs(context.layout, config, agents);
    let names = KnownNames::offline(config, &refs);
    let needed = needs_servers(&refs, ignore, &names);
    let listings = server_listings(context, config, needed).await;
    let names = names.with_listings(config, &listings);
    let checker = Checker::new(context.layout, config, &listings, &names);
    checker.check_tool_refs(&refs, ignore, context.options.check_tools, report);
}

/// Whether any reference is left that only a server could settle.
fn needs_servers(refs: &[ToolRef], ignore: &GlobSet, names: &KnownNames) -> bool {
    refs.iter()
        .any(|r| !ignore.is_match(&r.selector) && !names.matches(r))
}

/// Every enabled server's listing, run only when `needed`; with
/// `--tools skip`, placeholders that run nothing.
async fn server_listings(
    context: &CheckContext<'_>,
    config: &Config,
    needed: bool,
) -> HashMap<String, ServerListing> {
    if !context.options.check_tools {
        skipped_listings(config)
    } else if needed {
        list_servers(&config.tool_servers, context.options.list_timeout).await
    } else {
        HashMap::new()
    }
}

/// With `--tools skip`, every enabled server is unlisted.
fn skipped_listings(config: &Config) -> HashMap<String, ServerListing> {
    config
        .tool_servers
        .iter()
        .filter(|server| server.enabled)
        .map(|server| {
            let reason = "not run: --tools skip".to_string();
            (server_key(server), ServerListing::Unverifiable { reason })
        })
        .collect()
}

/// What one reference resolves to.
enum Outcome {
    Known,
    /// Not found, but a server that couldn't be listed may supply it.
    Unverified(Vec<String>),
    /// A literal on a listed server's prefix that the server doesn't list.
    NotOnServer {
        server: String,
        hint: Option<String>,
    },
    Unknown,
    GlobMatchesNothing,
}

/// An enabled server and the names it registers under: the one its listing
/// reports, or for a server that couldn't be listed, those its config gives
/// ([`configured_names`]), which are none for an unknown command.
struct ServerView<'a> {
    server: &'a ToolServerConfig,
    key: String,
    names: Vec<String>,
    listed: bool,
}

struct Checker<'a> {
    layout: &'a ConfigLayout,
    config: &'a Config,
    listings: &'a HashMap<String, ServerListing>,
    names: &'a KnownNames,
    servers: Vec<ServerView<'a>>,
}

impl<'a> Checker<'a> {
    fn new(
        layout: &'a ConfigLayout,
        config: &'a Config,
        listings: &'a HashMap<String, ServerListing>,
        names: &'a KnownNames,
    ) -> Self {
        let servers = config
            .tool_servers
            .iter()
            .filter(|server| server.enabled)
            .map(|server| {
                let key = server_key(server);
                let (names, listed) = match listings.get(&key) {
                    Some(ServerListing::Listed(listing)) => (vec![listing.server.clone()], true),
                    _ => (configured_names(server), false),
                };
                ServerView {
                    server,
                    key,
                    names,
                    listed,
                }
            })
            .collect();
        Self {
            layout,
            config,
            listings,
            names,
            servers,
        }
    }

    /// One finding per reference that names nothing, and one per server
    /// that couldn't be listed, counting the references it leaves unchecked.
    /// With `findings` off (`--tools skip`), only the counts.
    fn check_tool_refs(
        &self,
        refs: &[ToolRef],
        ignore: &GlobSet,
        findings: bool,
        report: &mut Report,
    ) {
        let mut unverified: BTreeMap<String, usize> = BTreeMap::new();
        for tool_ref in refs {
            if ignore.is_match(&tool_ref.selector) {
                report.summary.tools_ignored += 1;
                continue;
            }
            match self.outcome(tool_ref) {
                Outcome::Known => {}
                Outcome::Unverified(servers) => {
                    report.summary.tools_unverified += 1;
                    for server in servers {
                        *unverified.entry(server).or_default() += 1;
                    }
                }
                outcome if findings => report.push(self.reference_finding(tool_ref, outcome)),
                // With `--tools skip` nothing was listed, so a name that
                // matched no known prefix is unverified rather than wrong.
                _ => report.summary.tools_unverified += 1,
            }
        }
        for (key, count) in unverified.into_iter().filter(|_| findings) {
            self.report_unverified(&key, count, report);
        }
    }

    fn outcome(&self, tool_ref: &ToolRef) -> Outcome {
        if self.names.matches(tool_ref) {
            return Outcome::Known;
        }
        let candidates = self.candidates(tool_ref);
        let mut unverifiable: Vec<String> = candidates
            .iter()
            .filter(|view| !view.listed)
            .map(|view| view.key.clone())
            .collect();
        if candidates.is_empty() {
            // A server whose name is unknown could have any prefix.
            unverifiable = self
                .servers
                .iter()
                .filter(|view| !view.listed && view.names.is_empty())
                .map(|view| view.key.clone())
                .collect();
        }
        if !unverifiable.is_empty() {
            return Outcome::Unverified(unverifiable);
        }
        if is_glob(&tool_ref.selector) {
            return Outcome::GlobMatchesNothing;
        }
        match candidates.first() {
            Some(view) => Outcome::NotOnServer {
                server: self.server_label(view.server),
                hint: self.suggestion(tool_ref, &candidates),
            },
            None => Outcome::Unknown,
        }
    }

    /// The servers whose tools a reference could name: those whose name, as
    /// the reference's package sees it, it could match, and among servers
    /// with no known name, those the worker would start for it.
    fn candidates(&self, tool_ref: &ToolRef) -> Vec<&ServerView<'a>> {
        let package = tool_ref.agent_package.as_deref();
        let by_stem: Vec<String> = reach(self.config, tool_ref)
            .iter()
            .map(server_key)
            .collect();
        self.servers
            .iter()
            .filter(|view| match view.names.as_slice() {
                [] => by_stem.contains(&view.key),
                names => names.iter().any(|name| {
                    let registration = registration(view.server, name, Vec::new());
                    let prefix = ServerIdentity::agent_visible_name(package, &registration, "");
                    let display = prefix.strip_suffix('_').unwrap_or(&prefix);
                    selector_could_match_server(&tool_ref.selector, display)
                }),
            })
            .collect()
    }

    /// `'time'`, or `'time' (listed as 'clock')` for a renamed server.
    fn server_label(&self, server: &ToolServerConfig) -> String {
        let key = server_key(server);
        match self.listings.get(&key) {
            Some(ServerListing::Listed(listing)) if listing.server != server.name => {
                format!("'{key}' (listed as '{}')", listing.server)
            }
            _ => format!("'{key}'"),
        }
    }

    /// The closest tool name the reference's servers list, if close enough.
    fn suggestion(&self, tool_ref: &ToolRef, servers: &[&ServerView<'_>]) -> Option<String> {
        let package = tool_ref.agent_package.as_deref();
        servers
            .iter()
            .filter_map(|view| listed_registration(view.server, self.listings))
            .flat_map(|registration| visible_names(package, &registration))
            .map(|name| (strsim::levenshtein(&tool_ref.selector, &name), name))
            .filter(|(distance, _)| *distance <= MAX_SUGGESTION_DISTANCE)
            .min()
            .map(|(_, name)| name)
    }

    fn reference_finding(&self, tool_ref: &ToolRef, outcome: Outcome) -> Finding {
        let entry = format!("{} entry '{}'", tool_ref.key, tool_ref.selector);
        let finding = match outcome {
            Outcome::NotOnServer { server, hint } => {
                let hint = hint.map(|name| format!("; did you mean {name}?")).unwrap_or_default();
                Finding::of(
                    Code::UnknownTool,
                    format!("{entry} is not a tool that tool server {server} lists{hint}"),
                )
            }
            Outcome::GlobMatchesNothing => Finding::of(
                Code::GlobMatchesNothing,
                format!("{entry} matches no built-in, script, sub-agent or handoff tool, and no tool a configured server lists"),
            ),
            _ => Finding::of(
                Code::UnknownTool,
                format!("{entry} is not a built-in, script, sub-agent or handoff tool, and no configured tool server's name is its prefix"),
            ),
        };
        finding
            .in_file(self.layout.relative(&tool_ref.source))
            .at_key(tool_ref.key)
    }

    /// A server whose command is missing already has its `command-not-found`
    /// warning; its references still count as unverified.
    fn report_unverified(&self, key: &str, count: usize, report: &mut Report) {
        let Some(server) = self
            .config
            .tool_servers
            .iter()
            .find(|s| server_key(s) == key)
        else {
            return;
        };
        let Some(ServerListing::Unverifiable { reason }) = self.listings.get(key) else {
            return;
        };
        if command_missing(server) {
            return;
        }
        let references = if count == 1 {
            "1 tool reference".to_string()
        } else {
            format!("{count} tool references")
        };
        let finding = Finding::of(
            Code::ToolUnverified,
            format!("tool server '{key}' couldn't list its tools ({reason}), so {references} to it can't be checked"),
        );
        report.push(match tool_server_file(self.layout, server) {
            Some(file) => finding.in_file(self.layout.relative(&file)),
            None => finding,
        });
    }
}
