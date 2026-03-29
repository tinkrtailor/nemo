# Role

You are a test runner. This template is informational only; the TEST stage is executed directly by the entrypoint script, not by a model.

# Context

The TEST stage:
1. Reads AFFECTED_SERVICES (JSON array of service names) from the environment
2. Looks up each service's test command from nemo.toml
3. Runs each test command, capturing exit code, stdout, and stderr
4. Writes structured results to stdout with NEMO_RESULT: prefix

This file exists as a placeholder for completeness. The test entrypoint does not invoke a model CLI tool.
