export default function Home() {
  return <main>
    <nav><a className="brand" href="/"><span className="mark small">c</span> crumb</a><div><a href="https://github.com/elixpo/crumb.elixpo">GitHub</a><a className="nav-cta" href="/connect">Connect</a></div></nav>
    <section className="hero">
      <p className="eyebrow">Native first. Intelligence when asked.</p>
      <h1>Your shell stays yours.</h1>
      <p className="lede">Run every command exactly as you do today. Ask in natural language when you want help, and let skills bring the right model and tools.</p>
      <div className="actions"><a className="primary" href="https://github.com/elixpo/crumb.elixpo">Get Crumb</a><a className="secondary" href="/connect">Connect account</a></div>
      <div className="terminal">
        <div className="terminal-bar"><i></i><i></i><i></i><span>crumb — ~/workspace</span></div>
        <pre><span className="muted">╭─[ crumb ]─[ ~/workspace ]─[ linux ]</span>{'\n'}<b>╰─❯</b> cargo test{`\n`}<span className="success">   Finished</span> all tests passed{`\n\n`}<b>╰─❯</b> <span className="prompt">?</span> explain the failing migration</pre>
      </div>
    </section>
    <section className="features"><article><b>01</b><h2>A real terminal</h2><p>Bash, Zsh, and PowerShell keep native command semantics.</p></article><article><b>02</b><h2>Skills, not guesses</h2><p>Each skill declares its model, tools, permissions, and activation rules.</p></article><article><b>03</b><h2>Safer connections</h2><p>Connect providers in the browser. Tokens never travel through terminal history.</p></article></section>
    <footer><span>crumb.elixpo</span><span>Built in the open.</span></footer>
  </main>
}
