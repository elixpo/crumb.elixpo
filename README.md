<h1 align="center">
  <img src="crumb.elixpo/public/favicon.png" width="30" height="30" alt="Crumb panda" />
  Crumb
</h1>

<p>
  <img src="crumb.elixpo/public/og-image.png" alt="Crumb — the autonomous agent harness and natural language terminal" width="100%" />
</p>

> Crumb is Elixpo's self-hosted autonomous agent harness and natural language terminal, powered by Pollinations.

Crumb runs locally as a Rust application around your real Bash, Zsh, or PowerShell session. Use the commands you already know, or give Crumb a goal and let its native harness plan, use approved tools, retain bounded context, supervise parallel work, and carry the task forward. Pollinations supplies the default model intelligence; Crumb owns the execution loop, permissions, memory, tools, and user-visible evidence.

## A native autonomous harness

- Run ordinary commands without changing how your shell works.
- Give the Crumb agent a complete task and steer it while it works.
- Run autonomous multi-step work inside explicit user-granted boundaries.
- Keep the harness, sessions, memory, tools, and policy on your own machine.
- Use Pollinations models without making the terminal depend on a network connection.
- Add focused skills for the jobs you do often.
- Connect outside services only when you choose.
- Keep sessions, context, tool activity, checkpoints, and progress visible.

## Calm by default

Autonomous does not mean unaccountable. Crumb applies deterministic risk classes and user-owned permissions before a model-proposed action can reach a tool. It shows what is connected, asks before sensitive actions, supports cancellation and checkpoints, and keeps the terminal useful when Pollinations or any optional integration is unavailable.

Nothing should install itself, grant itself access, or get between you and the command line.

## Crumb and Elixpoo are different systems

Crumb is a user-operated autonomous development agent and terminal harness. [Elixpoo](https://agent.elixpo.com) is Elixpo's bounded GitHub project-management agent and never implements product code. They share an emphasis on inspectable permissions, but they have separate runtimes, responsibilities, and trust boundaries.

## A growing ecosystem

The Crumb marketplace is being shaped as a home for community-built skills, connectors, and MCP tools. Add-ons will be easy to discover and connect while remaining inspectable, removable, and separate from Crumb itself.

Explore the ideas behind [skills](https://crumb.elixpo.com/skills), [plugins](https://crumb.elixpo.com/plugins), and [connectors](https://crumb.elixpo.com/profile/connectors).

## Current status

Crumb is an active pre-release project. Its native harness, Pollinations integration, persistent shell, permission system, sessions, memory, MCP boundary, background work, and agent supervision are being developed in public. Public installers and downloadable terminal builds are not available yet; Windows, Linux, and macOS releases will follow release-readiness and security checks.

Follow the [development roadmap](docs/development/TODO.md), read the [documentation](https://crumb.elixpo.com/docs), or share an idea through [GitHub Issues](https://github.com/elixpo/crumb.elixpo/issues).

---

Made with care for people who live in the terminal.
