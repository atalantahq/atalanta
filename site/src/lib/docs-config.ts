/**
 * The docs registry. Every docs/*.md file must be listed here or in IGNORED,
 * or the build fails. Paths are relative to the repo root.
 * A slug of '' is /docs/ itself.
 */
export interface PageDef { label: string; slug: string; source: string }
export interface GroupDef { title: string; pages: PageDef[] }

export const GROUPS: GroupDef[] = [
  { title: 'Start here', pages: [
    { label: 'Overview', slug: '', source: 'README.md' },
    { label: 'Guide', slug: 'guide', source: 'docs/GUIDE.md' },
  ]},
  { title: 'Writing tests', pages: [
    { label: 'Writing suites', slug: 'dsl', source: 'docs/DSL.md' },
    { label: 'DSL reference', slug: 'dsl-reference', source: 'docs/DSL-REFERENCE.md' },
  ]},
  { title: 'Running tests', pages: [
    { label: 'CLI', slug: 'cli', source: 'docs/CLI.md' },
    { label: 'CI', slug: 'ci', source: 'docs/CI.md' },
    { label: 'Drivers', slug: 'drivers', source: 'docs/DRIVERS.md' },
  ]},
  { title: 'AI and agents', pages: [
    { label: 'From a coding agent', slug: 'agents', source: 'docs/AGENTS.md' },
    { label: 'MCP server', slug: 'mcp', source: 'docs/MCP.md' },
    { label: 'Explore', slug: 'explore', source: 'docs/EXPLORE.md' },
    { label: 'AI providers', slug: 'ai-providers', source: 'docs/AI-PROVIDERS.md' },
  ]},
  { title: 'Trust', pages: [
    { label: 'Security', slug: 'security', source: 'docs/SECURITY.md' },
    { label: 'Security policy', slug: 'security-policy', source: 'SECURITY.md' },
    { label: 'Verification status', slug: 'status', source: 'docs/STATUS.md' },
    { label: 'Benchmarks', slug: 'benchmarks', source: 'docs/BENCHMARKS.md' },
  ]},
  { title: 'Project', pages: [
    { label: 'Architecture', slug: 'architecture', source: 'docs/ARCHITECTURE.md' },
    { label: '0.2.0 changes', slug: 'changelog-0-2-0', source: 'docs/CHANGELOG-0.2.0.md' },
    { label: 'Roadmap', slug: 'roadmap', source: 'docs/ROADMAP.md' },
    { label: 'Changelog', slug: 'changelog', source: 'CHANGELOG.md' },
    { label: 'Contributing', slug: 'contributing', source: 'CONTRIBUTING.md' },
    { label: 'Support', slug: 'support', source: 'SUPPORT.md' },
  ]},
];

/** docs/*.md files that are deliberately not published. */
export const IGNORED = ['docs/README.md'];
