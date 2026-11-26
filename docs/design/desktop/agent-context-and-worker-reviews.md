# OX agent context and worker reviews

Proposed against n0rpan/ox at `be347b0`. Reviewed on the author's Claude subscription by Fable 5.2 in read-only OX worker `4177928940944705`.

## What the user should experience

A user can start a conversation in OX Desktop, continue it from their phone, and ask “where do I change providers here?” The agent knows which interface sent that question and gives directions for the installed OX version. “Have Fable review this on my subscription” launches a read-only reviewer. The user sees the reviewer under the conversation that requested it, even when the reviewer runs in a separate checkout. Opening the worker shows its model, billing route, checkout and a link back to the parent.

## Findings from using OX

| Observed | Consequence for the design |
| --- | --- |
| This session exposes OX worker tools. The Codex handshake identifies OX Desktop, but the turn builder supplies no explicit submitting interface to the model. | Supply a short model-visible statement with the current send. Tool availability alone cannot identify the interface. |
| The native Claude picker on this account listed Fable 5.2 and resolved its default to Opus 5.5. The worker tool accepts a model as an API alias and selects the Claude subscription when it is omitted. The catalog's Fable search returned API rows only. | Model discovery and launch must distinguish the exact model from who pays. Expose account-specific subscription choices. |
| For this review, a disposable checkout-local Claude model preference and a separate OX launcher were needed to pin Fable on the subscription using the existing worker API. The OX job reported “Claude subscription”. The native worker transcript identified the answering model as `claude-fable-5-2`. | Support model and checkout directly on a call from the current thread. Return the runtime-confirmed model in status. |
| In OX Desktop, “Independently revie…” appeared as a sibling above “Plan Ox Session Co…”. The job ledger stamped an external launcher with the clone as its project and a null parent scope. | Preserve the initiating conversation independently of where the worker executes. This observation does not establish a defect in normal same-project hosted nesting. |
| On a local mock provider, Codex 0.171.0 and 0.172.0 delivered a desktop marker and then a phone marker as developer context. User text stayed exact and the markers were absent from visible thread history. Claude 2.1.332 delivered a system append and prompt-submit hook context to the mock provider. | Use these probes as the starting fixtures. They establish runtime delivery. Host queue ordering, compaction and the Claude control-channel integration still need coverage. |

A second dogfood run reproduced the flat sidebar with two workers launched directly from a hosted Codex conversation. Both jobs recorded that conversation as their parent. Desktop could lose the host's parent information when it chose a richer row from local history. The sidebar now merges lineage independently of display quality, and desktop and phone resolve the recorded parent across checkouts and billing routes.

The same run exposed repeated tool approval requests after the user had selected YOLO for the parent. An implementation worker that leaves its mode unspecified now inherits the verified parent mode automatically. An explicit read-only review stays read-only. The host records how it authorized the child. The stock Claude integration test exercises a worker writing a file in a second checkout without either session asking for approval.

Two more findings affected worker status and startup. A newly opened worker could briefly look finished before its first turn started, and a Claude writer could fail to launch when equivalent sandbox settings used a different JSON key order. The fixes track the first turn and compare the structured settings. The overlap warning also names the checkout and describes conditions at launch, so reading an old job does not imply another writer is still active.

## 1. Make the normal worker launch satisfy the review request

Extend `start_ox_worker` and the host launch request so the billing route, native subscription model and optional execution checkout are independently expressible. Keep existing API-alias calls and existing subscription-default calls working. For example, a Claude subscription request can supply `subscription_model = claude-fable-5-2` and a checkout path. Reject a conflicting API alias. Resolve the subscription model against the signed-in runtime's picker before sending the task. Report unavailable models and routes explicitly. A model-name search should return both matching subscription choices and matching API aliases with billing labels. Cache picker results per account and runtime, and refresh on an availability error.

Resolve the initiating thread from the current gateway's existing caller identity. The host owns its scope and session ID. Give the worker its own execution scope, validate the requested checkout against the caller's existing workspace permissions, and retain the inherited access ceiling. Have both hosted and supported standalone launchers pass the resolved native model to Claude before the first inference. Record the requested choice, billing route and actual model separately. Label a model as pending until the runtime confirms it. Include the actual value in status, the completion record and the worker view.

Reuse the existing worker tree. Desktop currently looks up a parent under the child's project. Use the worker's full recorded parent scope and session key for nesting, selection and navigation across projects. Check the phone catalog when parent and child belong to different projects or routes. The launching thread should show a visible child or expandable count on start, retain it on completion, and recover the same tree after restart. When the parent is outside a filtered view, show the worker with a navigable parent reference. Keep genuine external launches identifiable by their recorded provenance.

Relevant code includes [worker tools](../../../src/workers.rs), [launch request and lineage](../../../desktop/ox-desk-core/src/origin.rs), [host launch and projection](../../../desktop/ox-host/src/engine.rs), [desktop tree](../../../desktop/ox-desktop/src/views/host/discovery.rs) and [phone tree](../../../desktop/ox-host-web/assets/app.js).

## 2. Identify the sender of each accepted message

Negotiate the ability to declare a surface after the existing host hello. The wire rejects unknown fields, so new clients must wait for the host to advertise support. The desktop declares its surface, and the authenticated phone bridge supplies the phone surface itself. A legacy client gets generic OX guidance. The model needs only a short surface label. Do not include device identifiers, credentials or account details.

The host stamps each accepted send from its submitting connection. Carry that stamp alongside the text and images through queuing, deferred resume and mid-turn steering. Bind the original stamp to the first accepted request ID across retries. Keep it outside the existing action-and-approval digest. Derive worker or parent-agent input from the actual internal dispatch path. Keep session lineage in the existing immutable worker origin record, and keep authorization decisions independent of this descriptive label.

## 3. Deliver just enough context to answer the question

Example for a human turn: “This message was sent from OX Desktop. This session uses Codex on the ChatGPT subscription. For OX settings, use the installed CLI's ox help settings.” A worker receives its role and current billing route. A cross-runtime handoff uses the destination runtime and the actual initiating surface. Historical statements in carried messages retain their historical meaning. A client using only the shared relay receives no OX Desktop claim.

For Codex, attach a small application entry under a stable key in `additionalContext` on both `turn/start` and `turn/steer`. OX already enables the experimental capability it requires. Verify effective context on resume and after compaction. T3 contains explicit restoration logic for a compacted thread. Add restoration only if stock-runtime probes demonstrate that OX loses the current value, and check that restoring it cannot trigger an extra turn.

For Claude, append the stable OX facts once per process. Prototype a `UserPromptSubmit` hook registered through the existing stream-JSON initialize exchange. Match the callback to the accepted submit so a queued phone message keeps its phone label when the desktop is active. Probe the supported floor, 2.1.331, and the current version for callback correlation, ordering under interrupt and replay filtering before selecting this transport. The earlier probe used a settings hook directly with stock Claude. OX subscription launches already own the settings overlay, so adding a second settings flag would break that path. If reliable per-submit delivery is unavailable, retain generic OX context on Claude until a stock-compatible transport passes the same tests.

Add `ox help settings` alongside the existing setup and desktop help. Cover the desktop and phone navigation for providers, accounts and remote access, with links to existing setup and doctor commands. Use the command as an on-demand source so unrelated coding prompts stay short.

## 4. Let people ask in their own words

| User wording | Expected behavior |
| --- | --- |
| “What do you think?” or “Review your plan.” | Give the current agent's assessment. |
| “I'm worried we missed something.” | Address the concern. When an independent review would materially help, suggest one concrete option, such as “I can ask Fable on your Claude subscription to check the cross-device cases.” |
| “Get a second opinion.” | Treat this as authorization for one read-only independent review. Use an established reviewer or subscription preference and identify the selection. If the route is ambiguous and the choice changes who pays, ask one short billing question. |
| “Have Fable 5.2 check this on my subscription.” | Launch the exact subscription model directly from the current conversation, follow it to completion, verify the actual model and summarize the changes its review warrants. |
| “How would I get another model to review this?” | Explain the available route. This wording asks for instructions. |

Expose these distinctions to the main agent with OX's worker capability description. Human intent selects the action. A tool name, slash command or magic phrase should never be required.

## Acceptance checks

1. From a visible hosted Codex conversation, request a Fable 5.2 subscription review in a separate checkout. Before the first review turn, assert the chosen billing route and model. Verify the first assistant message's actual model, the job ID and a read-only access ceiling. Supplying an unavailable model must produce an actionable result without spending on another route.
2. Open desktop and phone on the same parent. Verify one child visually nested under that exact parent, a working return link, a running indicator that clears on completion, and the same relationship after reconnect, host restart and catalog refresh. A second busy thread in the same project must acquire no child. Include a real external launch as a control.
3. Start on desktop, send from phone, queue during a busy turn, steer from the opposite client, and retry a previously accepted request after reconnect. Inspect actual model requests and displayed transcripts for the correct sender, a single delivery, intact user text and a clean conversation history.
4. Repeat after compaction, resume, fork and a cross-runtime handoff. Check both supported release floors, subscription and API routes, worker prompts, old clients and a relay-only consumer. Preserve prompt and sandbox behavior when a context feature is unavailable.
5. Ask “which app am I using?” and “where do I add a provider here?” on both interfaces. Verify the instructions against the installed UI. Exercise the natural-language examples above and check actual job counts and billing outcomes.

The first shippable slice is the end-to-end review in checks 1 and 2. It eliminates the workaround observed in this session. Sender attribution and version-matched settings help can then ship against checks 3 through 5.
