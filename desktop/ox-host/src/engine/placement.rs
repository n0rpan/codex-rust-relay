//! Resolve a worker destination on the parent before a launch creates a job.
use super::*;

pub(super) enum HostQuery {
    List(Replier),
    Place {
        caller: Caller,
        start: Box<WorkerStart>,
        selector: String,
        prefer_parent: bool,
        level: Level,
        reply: Replier,
    },
}

impl Engine {
    /// Check policy, then hand the launch to the worker's own runtime. Nothing
    /// starts unless every check passes. The reply waits for the native session,
    /// or, for a YOLO child that needs the operator, returns at once with
    /// `waiting_for_operator`.
    pub(super) fn launch(
        &mut self,
        mut caller: Caller,
        mut start: WorkerStart,
        reply: Replier,
    ) -> io::Result<()> {
        if self.update_reserved() {
            return self.answer(reply, Err(crate::handover::REFUSAL.into()));
        }
        let mut prepare = || -> Result<_, String> {
            start.validate()?;
            let (parent_project, parent, level, external) = self.worker_source(&caller)?;
            if let Caller::Hosted { hint, .. } = &mut caller {
                *hint = parent;
            }
            let project = ox_desk_core::worker_checkout::resolve(
                &parent_project,
                start.checkout.as_deref(),
                level == Some(Level::Yolo),
            )?;
            if start.checkout.is_some() {
                start.checkout = Some(project.clone());
            }
            let mut selector = start.host.take();
            let mut prefer_parent = false;
            if selector.is_none() && start.needs.is_none() {
                let rule =
                    crate::worker_placement::rule(&parent_project, start.placement.as_deref())?;
                selector = rule.host;
                if let Some(os) = rule.os {
                    prefer_parent = selector.is_none();
                    start.needs.get_or_insert_with(Default::default).os = Some(os);
                }
            }
            start.placement = None;
            let selector = selector.unwrap_or_else(|| {
                if start.needs.is_some() {
                    "auto"
                } else {
                    "parent"
                }
                .into()
            });
            let level = level.unwrap_or(crate::workers::TERMINAL_CEILING);
            if external
                && start.mode_request() == ox_desk_core::modes::ModeRequest::Inherit
                && (selector != "parent" || start.needs.is_some())
            {
                let resolved =
                    crate::workers::grant(None, true, start.mode_request(), start.runtime)?;
                start.permission = Some(ox_desk_core::modes::ModeRequest::Level(resolved.level));
            }
            Ok((project, selector, level, prefer_parent))
        };
        let (project, selector, level, prefer_parent) = match prepare() {
            Ok(p) => p,
            Err(e) => return self.answer(reply, Err(e)),
        };
        if (selector != "parent" && uuid::Uuid::parse_str(&selector).is_err())
            || start.needs.is_some()
        {
            return self.query_hosts(
                project,
                HostQuery::Place {
                    caller,
                    start: Box::new(start),
                    selector,
                    prefer_parent,
                    level,
                    reply,
                },
            );
        }
        self.launch_on(caller, start, selector, reply)
    }

    fn launch_on(
        &mut self,
        caller: Caller,
        start: WorkerStart,
        host: String,
        reply: Replier,
    ) -> io::Result<()> {
        if self.update_reserved() || self.draining.is_some() {
            return self.answer(reply, Err(crate::handover::REFUSAL.into()));
        }
        if host != "parent" && host != self.host_id.to_string() {
            let result = self.forward_launch(caller, start, &host);
            return self.answer(reply, result);
        }
        let accepted = self.accept_launch(&caller, start);
        self.start_accepted(accepted, reply)
    }
    pub(super) fn list_hosts(&mut self, caller: &Caller, reply: Replier) -> io::Result<()> {
        use crate::workers::Resolved;
        let project = match self.resolve(caller) {
            Ok(Resolved::Hosted { scope, .. }) => scope.project,
            Ok(Resolved::Terminal { project, .. }) => project,
            Ok(Resolved::Linked { .. }) => {
                return self.answer(reply, Err("Query worker hosts on the parent host.".into()));
            }
            Err(error) => return self.answer(reply, Err(error)),
        };
        self.query_hosts(project, HostQuery::List(reply))
    }
    pub(super) fn query_hosts(&mut self, project: PathBuf, query: HostQuery) -> io::Result<()> {
        if let Some(waiting) = self.host_queries.get_mut(&project) {
            waiting.push(query);
            return Ok(());
        }
        self.host_queries.insert(project.clone(), vec![query]);
        let host = self.host_info();
        let config = self.config.clone();
        let registry = self.state_dir.join("worker-links.json");
        let tx = self.tx.clone();
        thread::spawn(move || {
            let runtimes = config.installed_runtimes();
            let prepared = config.prepare();
            let routes: Vec<_> = prepared
                .as_ref()
                .into_iter()
                .flat_map(|p| {
                    p.routes.iter().filter(|r| {
                        runtimes.contains(&r.runtime) && (!r.is_api() || p.has_api_provider())
                    })
                })
                .collect();
            let local = json!({"host":host, "parent_host":true, "status":"online", "runtimes":runtimes,
                "routes":routes, "codex_subscription_workers":true, "routes_known":prepared.is_ok(), "has_project":true});
            let result = ox_host_web::inventory::list(
                &registry,
                &project,
                host.id.as_deref().unwrap_or_default(),
                local,
            )
            .map(|mut list| {
                match crate::worker_placement::rule(&project, None) {
                    Ok(rule) if rule.host.is_some() || rule.os.is_some() => {
                        list["project_placement"] = json!({"host":rule.host,"os":rule.os});
                    }
                    Err(e) => list["placement_error"] = e.into(),
                    _ => (),
                }
                list
            })
            .map_err(|e| e.to_string());
            let _ = tx.send(Message::HostsListed(project, result));
        });
        Ok(())
    }
    pub(super) fn hosts_listed(
        &mut self,
        project: PathBuf,
        result: Result<Value, String>,
    ) -> io::Result<()> {
        for query in self.host_queries.remove(&project).unwrap_or_default() {
            match query {
                HostQuery::List(reply) => self.answer(reply, result.clone())?,
                HostQuery::Place {
                    caller,
                    mut start,
                    selector,
                    prefer_parent,
                    level,
                    reply,
                } => {
                    let needs = start.needs.take().unwrap_or_default();
                    let selected = result.as_ref().map_err(Clone::clone).and_then(|v| {
                        ox_host_web::inventory::choose(
                            v,
                            &selector,
                            &needs,
                            &start,
                            level,
                            prefer_parent,
                        )
                    });
                    match selected {
                        Ok(id) => self.launch_on(caller, *start, id, reply)?,
                        Err(e) => self.answer(reply, Err(e))?,
                    }
                }
            }
        }
        Ok(())
    }
}
