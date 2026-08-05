# End-User CLI Extension Journey Report

> Generated: 2026-08-04T22:36:17Z

## Summary

| Metric | Value |
|--------|-------|
| Must-pass total | 125 |
| Tested | 125 |
| Passed | 105 |
| Failed | 20 |
| Skipped | 0 |
| Pass rate | 84.0% |

## By Journey Category

| Category | Pass | Fail | Skip |
|----------|------|------|------|
| command_provider | 33 | 0 | 0 |
| event_subscriber | 34 | 1 | 0 |
| multi_capability | 27 | 15 | 0 |
| passive | 0 | 2 | 0 |
| tool_provider | 11 | 2 | 0 |

## Journey Failures

### bash-spawn-hook (tier 1)

- **Category:** ToolProvider
- **Journey:** Load extension -> verify tool registration -> check tool schema
- **Failed at:** load_extension
- **Reason:** Extension 'bash-spawn-hook': runtime tools identities differ from the manifest; missing declared identities=[], unexpected runtime identities=["bash"], expected={}, actual={"bash"}
- **Progress:** 0/2 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_bash_spawn_hook --nocapture --exact
  ```

### community/hjanuschka-plan-mode (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'community/hjanuschka-plan-mode': runtime flags identities differ from the manifest; missing declared identities=[], unexpected runtime identities=["plan"], expected={}, actual={"plan"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_community_hjanuschka_plan_mode --nocapture --exact
  ```

### community/mitsuhiko-uv (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'community/mitsuhiko-uv': runtime tools identities differ from the manifest; missing declared identities=[], unexpected runtime identities=["bash"], expected={}, actual={"bash"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_community_mitsuhiko_uv --nocapture --exact
  ```

### community/qualisero-session-color (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'community/qualisero-session-color': event-handler identities differ from the manifest; expected={"resize", "session_start", "session_switch"}, actual={"session_start", "session_switch"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_community_qualisero_session_color --nocapture --exact
  ```

### community/tmustier-tab-status (tier 1)

- **Category:** Passive
- **Journey:** Load extension -> verify basic activation -> check no registration errors
- **Failed at:** load_extension
- **Reason:** Extension 'community/tmustier-tab-status': event-handler identities differ from the manifest; expected={}, actual={"agent_end", "agent_start", "before_agent_start", "session_shutdown", "session_start", "session_switch", "tool_call", "tool_result", "turn_start"}
- **Progress:** 0/1 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_community_tmustier_tab_status --nocapture --exact
  ```

### event-bus (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'event-bus': event-handler identities differ from the manifest; expected={"my:notification", "session_start"}, actual={"session_start"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_event_bus --nocapture --exact
  ```

### npm/benvargas-pi-ancestor-discovery (tier 1)

- **Category:** Passive
- **Journey:** Load extension -> verify basic activation -> check no registration errors
- **Failed at:** load_extension
- **Reason:** Extension 'npm/benvargas-pi-ancestor-discovery': event-handler identities differ from the manifest; expected={}, actual={"resources_discover"}
- **Progress:** 0/1 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_npm_benvargas_pi_ancestor_discovery --nocapture --exact
  ```

### npm/ogulcancelik-pi-sketch (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'npm/ogulcancelik-pi-sketch': event-handler identities differ from the manifest; expected={"data", "end", "error"}, actual={}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_npm_ogulcancelik_pi_sketch --nocapture --exact
  ```

### npm/pi-prompt-template-model (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'npm/pi-prompt-template-model': runtime commands identities differ from the manifest; missing declared identities=[], unexpected runtime identities=["model-mode"], expected={}, actual={"model-mode"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_npm_pi_prompt_template_model --nocapture --exact
  ```

### npm/vpellegrino-pi-skills (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'npm/vpellegrino-pi-skills': event-handler identities differ from the manifest; expected={"close", "data"}, actual={}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_npm_vpellegrino_pi_skills --nocapture --exact
  ```

### overlay-qa-tests (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'overlay-qa-tests': event-handler identities differ from the manifest; expected={"close", "data"}, actual={}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_overlay_qa_tests --nocapture --exact
  ```

### preset (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'preset': runtime flags identities differ from the manifest; missing declared identities=[], unexpected runtime identities=["preset"], expected={}, actual={"preset"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_preset --nocapture --exact
  ```

### ssh (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'ssh': runtime flags identities differ from the manifest; missing declared identities=[], unexpected runtime identities=["ssh"], expected={}, actual={"ssh"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_ssh --nocapture --exact
  ```

### third-party/graffioh-pi-screenshots-picker (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'third-party/graffioh-pi-screenshots-picker': runtime commands identities differ from the manifest; missing declared identities=[], unexpected runtime identities=["calendar", "dcp-debug", "dcp-init", "dcp-logs", "dcp-recent", "dcp-stats", "dcp-toggle", "dcp-tools", "scurl", "scurl-history", "scurl-log", "sketch", "slow-mode"], expected={"ss", "ss-clear"}, actual={"calendar", "dcp-debug", "dcp-init", "dcp-logs", "dcp-recent", "dcp-stats", "dcp-toggle", "dcp-tools", "scurl", "scurl-history", "scurl-log", "sketch", "slow-mode", "ss", "ss-clear"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_third_party_graffioh_pi_screenshots_picker --nocapture --exact
  ```

### third-party/graffioh-pi-super-curl (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'third-party/graffioh-pi-super-curl': runtime commands identities differ from the manifest; missing declared identities=[], unexpected runtime identities=["calendar", "dcp-debug", "dcp-init", "dcp-logs", "dcp-recent", "dcp-stats", "dcp-toggle", "dcp-tools", "sketch", "slow-mode", "ss", "ss-clear"], expected={"scurl", "scurl-history", "scurl-log"}, actual={"calendar", "dcp-debug", "dcp-init", "dcp-logs", "dcp-recent", "dcp-stats", "dcp-toggle", "dcp-tools", "scurl", "scurl-history", "scurl-log", "sketch", "slow-mode", "ss", "ss-clear"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_third_party_graffioh_pi_super_curl --nocapture --exact
  ```

### third-party/jyaunches-pi-canvas (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'third-party/jyaunches-pi-canvas': runtime commands identities differ from the manifest; missing declared identities=[], unexpected runtime identities=["ad-name", "ask", "askwith", "assistant", "branch", "code", "codemap", "command-center", "dcp-debug", "dcp-init", "dcp-logs", "dcp-recent", "dcp-stats", "dcp-toggle", "dcp-tools", "dumb-zone-status", "end-review", "ephemeral", "extensions:update", "files", "followup", "handoff", "md", "model-sysprompt-appendix", "notify", "oracle", "plan", "plan:list", "plan:save", "preset", "processes", "providers:toggle-widget", "providers:usage", "review", "rp-tools-lock", "rpbind", "sandbox", "scurl", "scurl-history", "scurl-log", "session-ask", "session:copy-id", "session:copy-path", "sketch", "skill", "speedread", "ss", "ss-clear", "steer", "sub-bar:import", "sub-bar:settings", "sub-core:settings", "subagent", "system-prompt", "theme", "todos", "tools", "ultrathink", "usage", "vog", "ws"], expected={"calendar"}, actual={"ad-name", "ask", "askwith", "assistant", "branch", "calendar", "code", "codemap", "command-center", "dcp-debug", "dcp-init", "dcp-logs", "dcp-recent", "dcp-stats", "dcp-toggle", "dcp-tools", "dumb-zone-status", "end-review", "ephemeral", "extensions:update", "files", "followup", "handoff", "md", "model-sysprompt-appendix", "notify", "oracle", "plan", "plan:list", "plan:save", "preset", "processes", "providers:toggle-widget", "providers:usage", "review", "rp-tools-lock", "rpbind", "sandbox", "scurl", "scurl-history", "scurl-log", "session-ask", "session:copy-id", "session:copy-path", "sketch", "skill", "speedread", "ss", "ss-clear", "steer", "sub-bar:import", "sub-bar:settings", "sub-core:settings", "subagent", "system-prompt", "theme", "todos", "tools", "ultrathink", "usage", "vog", "ws"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_third_party_jyaunches_pi_canvas --nocapture --exact
  ```

### third-party/lsj5031-pi-notification-extension (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'third-party/lsj5031-pi-notification-extension': event-handler identities differ from the manifest; expected={"agent_end", "data", "end", "error", "session_start", "timeout"}, actual={"agent_end", "session_start"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_third_party_lsj5031_pi_notification_extension --nocapture --exact
  ```

### third-party/ogulcancelik-pi-sketch (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** load_extension
- **Reason:** Extension 'third-party/ogulcancelik-pi-sketch': runtime commands identities differ from the manifest; missing declared identities=[], unexpected runtime identities=["calendar", "dcp-debug", "dcp-init", "dcp-logs", "dcp-recent", "dcp-stats", "dcp-toggle", "dcp-tools", "scurl", "scurl-history", "scurl-log", "slow-mode", "ss", "ss-clear"], expected={"sketch"}, actual={"calendar", "dcp-debug", "dcp-init", "dcp-logs", "dcp-recent", "dcp-stats", "dcp-toggle", "dcp-tools", "scurl", "scurl-history", "scurl-log", "sketch", "slow-mode", "ss", "ss-clear"}
- **Progress:** 0/3 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_third_party_ogulcancelik_pi_sketch --nocapture --exact
  ```

### third-party/rytswd-direnv (tier 2)

- **Category:** EventSubscriber
- **Journey:** Load extension -> verify event handler registration -> check subscriptions
- **Failed at:** load_extension
- **Reason:** Extension 'third-party/rytswd-direnv': manifest commands capability=false, but runtime registered 15 commands
- **Progress:** 0/2 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_third_party_rytswd_direnv --nocapture --exact
  ```

### third-party/rytswd-questionnaire (tier 1)

- **Category:** ToolProvider
- **Journey:** Load extension -> verify tool registration -> check tool schema
- **Failed at:** load_extension
- **Reason:** Extension 'third-party/rytswd-questionnaire': manifest commands capability=false, but runtime registered 15 commands
- **Progress:** 0/2 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_third_party_rytswd_questionnaire --nocapture --exact
  ```

