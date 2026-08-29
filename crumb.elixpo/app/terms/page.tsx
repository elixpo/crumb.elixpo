import type { Metadata } from 'next'
import { InfoPage } from '@/app/components/InfoPage'

export const metadata: Metadata = { title: 'Terms' }
export default function TermsPage() { return <InfoPage eyebrow="Legal" title="Terms" intro="Crumb is open-source, pre-release terminal software. Use it with the same care you apply to shell commands."><h2>Your responsibility</h2><p>You remain responsible for commands and actions you approve, including their effects on files, systems, accounts, and third-party services.</p><h2>Third-party services</h2><p>Connected providers such as Elixpo Accounts and Pollinations are also governed by their own terms and availability.</p><h2>No warranty</h2><p>The open-source software is provided under the warranty terms in its repository license. Hosted features may change while the project is in development.</p></InfoPage> }
