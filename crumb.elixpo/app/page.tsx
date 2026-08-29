import Link from 'next/link'
import { SiteFooter } from './components/SiteFooter'
import { SiteHeader } from './components/SiteHeader'
import { DynamicCta } from './components/DynamicCta'

export default function Home() {
  return <><SiteHeader /><main>
    <section className="hero">
      <div className="hero-glow" />
      <p className="status-pill"><span /> Native shell · optional intelligence</p>
      <h1>The terminal that<br /><em>speaks your language.</em></h1>
      <p className="hero-copy">Run commands as usual, or describe what you need in everyday language. Crumb brings in AI only for the work that needs it.</p>
      <div className="hero-actions"><a className="button" href="https://github.com/elixpo/crumb.elixpo">Get Crumb <span>↗</span></a><Link className="button button-secondary" href="/profile/connectors">Check Connectors</Link></div>
      <div className="trust-strip"><span>No shell replacement</span><i /><span>No keys in history</span><i /><span>Open source</span></div>
      <div className="terminal-window" aria-label="Crumb terminal preview">
        <div className="terminal-title"><div><i /><i /><i /></div><span>crumb — ~/projects/NLT</span><b>NLT</b></div>
        <pre><span className="dim">╭─[ crumb ]─[ ~/projects/NLT ]─[ linux ]─[ git:main ]</span>{'\n'}<span className="caret">╰─❯</span> <span className="ask">find why the API tests fail and fix the smallest issue</span>{'\n'}<span className="agent">◆ harness</span> qwen-coder · effort: high · skill: rust-maintainer{'\n'}<span className="tool">  ↳ read</span> Cargo.toml, crates/api/tests{'\n'}<span className="tool">  ↳ run </span><span className="command">cargo test -p crumb-api</span>{'\n'}<span className="red">    error[E0308]</span> mismatched status type at src/routes.rs:84{'\n'}<span className="tool">  ↳ patch</span> crates/api/src/routes.rs <span className="yes">approved</span>{'\n'}<span className="tool">  ↳ run </span><span className="command">cargo test -p crumb-api</span>{'\n'}<span className="green">    12 passed</span>; 0 failed{'\n\n'}<span className="agent">◆ done</span> Normalized the handler&apos;s status conversion; no API behavior changed.{'\n'}<span className="dim">  1 file changed · session saved · Ctrl+C cancels at any step</span></pre>
      </div>
    </section>

    <section className="section" id="features"><p className="kicker">A terminal first</p><div className="section-heading"><h2>AI help without changing how commands work.</h2><p>Your normal commands still run in Bash, Zsh, or PowerShell. Ask in plain language when you want Crumb to help with a larger task. <Link className="inline-arrow" href="/features">Explore every feature →</Link></p></div>
      <div className="feature-grid">
        <article><span>01</span><div className="feature-icon">$_</div><h3>Your shell still works</h3><p>Keep using the commands, programs, shortcuts, and full-screen tools you already know.</p></article>
        <article><span>02</span><div className="feature-icon">◆</div><h3>Ask for the outcome</h3><p>Describe a job in plain language and let a skill guide Crumb through the right steps.</p></article>
        <article><span>03</span><div className="feature-icon">⌁</div><h3>Connect accounts safely</h3><p>Sign in through the browser instead of pasting private keys into commands or chat.</p></article>
      </div>
    </section>

    <section className="boundary-section"><div><p className="kicker">You stay in charge</p><h2>See what Crumb is doing and stop it at any time.</h2><p>Commands run normally. AI tasks use a separate workspace with clear permissions, so the model cannot quietly give itself more access.</p><Link className="text-link" href="/docs">See how it works <span>→</span></Link></div><ol><li><b>1</b><span><strong>Type a command</strong>It runs in your real shell.</span></li><li><b>2</b><span><strong>Ask for help</strong>Crumb chooses the right skill and model.</span></li><li><b>3</b><span><strong>Review important steps</strong>You approve actions that can change your work.</span></li></ol></section>

    <section className="flow-section">
      <div className="flow-heading"><p className="kicker">One line, three steps</p><h2>Quick for commands.<br />Careful with bigger jobs.</h2></div>
      <div className="flow-track"><article><span>01</span><b>Crumb understands the input</b><p>Known commands go straight to your shell. Plain-language requests go to the agent.</p></article><i>→</i><article><span>02</span><b>The right setup is loaded</b><p>Crumb picks the instructions, model, tools, and context needed for the job.</p></article><i>→</i><article><span>03</span><b>You see the work happen</b><p>Actions run with clear permissions, live progress, and one cancellation shortcut.</p></article></div>
    </section>

    <section className="namespace-section">
      <div><p className="kicker">Designed for recall</p><h2>A small grammar for serious work.</h2><p>Write normally to ask. Use `/` for Crumb actions and `@` to bring precise context into the request.</p><Link className="text-link" href="/docs/cli">Learn the CLI grammar <span>→</span></Link></div>
      <div className="namespace-cards"><article><code>/</code><span><b>Control the terminal</b><small>/skills · /mode · /connectors · /doctor</small></span></article><article><code>@</code><span><b>Reference context</b><small>@file · @folder · @skill · @connector</small></span></article><article><code>⌃C</code><span><b>Stop the whole chain</b><small>Cancel the model, Harness, and child process.</small></span></article></div>
    </section>

    <section className="ecosystem-section"><div><p className="kicker">Make it yours</p><h2>Add the knowledge<br />and tools you need.</h2><p>Skills show Crumb how to approach a job. Plugins give it useful tools. Connectors let those tools use accounts you approve.</p><div><Link className="button" href="/skills">Explore skills</Link><Link className="button button-secondary" href="/plugins">Explore plugins</Link></div></div><aside><div><span>Skills</span><strong>Ways of working</strong><small>Clear · inspectable · reusable</small></div><div><span>Plugins</span><strong>Tools for the job</strong><small>Limited · permissioned · replaceable</small></div><div><span>Connectors</span><strong>Your approved accounts</strong><small>Secure · revocable · user-owned</small></div></aside></section>

    <DynamicCta />
  </main><SiteFooter /></>
}
