import Link from 'next/link'
import { SiteFooter } from './components/SiteFooter'
import { SiteHeader } from './components/SiteHeader'

export default function Home() {
  return <><SiteHeader /><main>
    <section className="hero">
      <div className="hero-glow" />
      <p className="status-pill"><span /> Native shell · optional intelligence</p>
      <h1>The terminal that<br /><em>speaks your language.</em></h1>
      <p className="hero-copy">Crumb is the Natural Language Terminal. Commands run natively; models, skills, and tools appear only when you ask for them.</p>
      <div className="hero-actions"><a className="button" href="https://github.com/elixpo/crumb.elixpo">Get Crumb <span>↗</span></a><Link className="button button-secondary" href="/connect">Connect Pollinations</Link></div>
      <div className="trust-strip"><span>No shell replacement</span><i /><span>No keys in history</span><i /><span>Open source</span></div>
      <div className="terminal-window" aria-label="Crumb terminal preview">
        <div className="terminal-title"><div><i /><i /><i /></div><span>crumb — ~/projects/lnt</span><b>LNT</b></div>
        <pre><span className="dim">╭─[ crumb ]─[ ~/projects/lnt ]─[ linux ]─[ git:main ]</span>{'\n'}<span className="caret">╰─❯</span> cargo test --workspace{'\n'}<span className="green">   Finished</span> test profile in 2.14s{'\n'}<span className="green">   24 passed</span>; 0 failed{'\n\n'}<span className="dim">╭─[ crumb ]─[ ~/projects/lnt ]─[ linux ]─[ git:main ]</span>{'\n'}<span className="caret">╰─❯</span> <span className="ask">fix the failing migration and explain the change</span>{'\n'}<span className="agent">◆ skill: database-debugger · qwen-coder</span>{'\n'}<span className="dim">  I found a missing rollback guard. Review the patch?</span> <span className="yes">[Y/n]</span></pre>
      </div>
    </section>

    <section className="section" id="features"><p className="kicker">Built around the shell</p><div className="section-heading"><h2>Intelligence without taking over.</h2><p>Your shell remains the source of truth. Crumb adds a deliberate AI layer around it—not inside it.</p></div>
      <div className="feature-grid">
        <article><span>01</span><div className="feature-icon">$_</div><h3>Native by default</h3><p>Bash, Zsh, and PowerShell retain their real semantics, processes, state, and shortcuts.</p></article>
        <article><span>02</span><div className="feature-icon">◆</div><h3>Skills over guesses</h3><p>Skills define the model, tools, context, and permissions needed for a task.</p></article>
        <article><span>03</span><div className="feature-icon">⌁</div><h3>Safe connections</h3><p>Bind providers in your browser and authorize devices without pasting secrets into a terminal.</p></article>
      </div>
    </section>

    <section className="boundary-section"><div><p className="kicker">A clear boundary</p><h2>You decide when AI enters the session.</h2><p>Every ordinary command stays ordinary. Natural language routes into an isolated agent environment with explicit permissions and provider-neutral interfaces.</p><Link className="text-link" href="/docs">Read how Crumb works <span>→</span></Link></div><ol><li><b>1</b><span><strong>Type a command</strong>It runs in your persistent native shell.</span></li><li><b>2</b><span><strong>Ask in plain English</strong>Crumb selects an applicable skill and model.</span></li><li><b>3</b><span><strong>Approve real actions</strong>Models cannot grant themselves permissions.</span></li></ol></section>

    <section className="cta"><p className="kicker">Your terminal, intelligently layered</p><h2>Keep the speed.<br />Add the language.</h2><div><a className="button button-light" href="https://github.com/elixpo/crumb.elixpo">Explore on GitHub</a><Link className="button button-outline-light" href="/connect">Connect your account</Link></div></section>
  </main><SiteFooter /></>
}
