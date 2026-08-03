# End-User CLI Extension Journey Report

> Generated: 2026-08-03T16:32:18Z

## Summary

| Metric | Value |
|--------|-------|
| Must-pass total | 123 |
| Tested | 122 |
| Passed | 121 |
| Failed | 1 |
| Skipped | 1 |
| Pass rate | 99.2% |

## By Journey Category

| Category | Pass | Fail | Skip |
|----------|------|------|------|
| command_provider | 33 | 0 | 0 |
| event_subscriber | 34 | 0 | 1 |
| multi_capability | 41 | 1 | 0 |
| passive | 2 | 0 | 0 |
| tool_provider | 11 | 0 | 0 |

## Journey Failures

### npm/pi-prompt-template-model (tier 2)

- **Category:** MultiCapability
- **Journey:** Load extension -> verify all registration types -> cross-check capabilities
- **Failed at:** verify_all_registrations
- **Reason:** Expected commands registration but none found
- **Progress:** 1/4 steps
- **Reproduce:**
  ```bash
  cargo test --test ext_conformance_generated --features ext-conformance -- ext_npm_pi_prompt_template_model --nocapture --exact
  ```

