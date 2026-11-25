// @ts-check
/** @typedef {import('../../ox-host-protocol/generated/host-v1').Client} Client */
/** @typedef {import('../../ox-host-protocol/generated/host-v1').Scope} Scope */
/** @typedef {import('../../ox-host-protocol/generated/host-v1').Cursor} Cursor */
/** @typedef {import('../../ox-host-protocol/generated/host-v1').Action} Action */
/** @returns {Client} */
export const hello = () => ({ type: "hello", version: 1 });
/** @param {string} id @returns {Client} */
export const discover = (id) => ({ type: "discover", id });
/** @param {Scope|null} scope @param {Cursor|null} after @returns {Client} */
export const subscribe = (scope, after) => ({
  type: "subscribe",
  scope,
  after,
});
/** @param {string} id @param {Scope} scope @param {Action} action @param {string|null} approval @returns {Client} */
export const act = (id, scope, action, approval) => ({
  type: "act",
  id,
  scope,
  action,
  approval,
});
/** @param {string} id @returns {Client} */
export const listProcesses = (id) => ({ type: "list_processes", id });
