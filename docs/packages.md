# Package Management

Pi supports installing packages that provide extensions, skills, prompt templates, and themes.

## Sources

Pi supports three types of package sources:

1. **npm**: `npm:package-name` or `npm:@org/package` (optionally `@version`)
2. **git**: `git:host/owner/repo` or just `https://github.com/owner/repo` (optionally `@ref`)
3. **local**: path to a directory (e.g. `../my-package`)

## Commands

### Install

Install a package globally (user scope):
```bash
pi install npm:pi-skills
pi install git:github.com/someuser/my-tools
```

Install locally for the current project:
```bash
pi install --local npm:@org/project-utils
```

This adds the package to your `settings.json` (global or project) and installs it.

### Remove

Remove a package:
```bash
pi remove npm:pi-skills
pi remove --local npm:@org/project-utils
```

### Update

Update all packages (or a specific one):
```bash
pi update
pi update npm:pi-skills
```

Packages with pinned versions (e.g. `npm:pkg@1.2.3` or `git:repo@v1`) are skipped unless the command arguments explicitly change the version.

### List

List installed packages:
```bash
pi list
```

## Resource Discovery

When a package is installed, Pi looks for resources in the following locations within the package root:

1. **Manifest**: If `package.json` has a `pi` section, it uses the paths defined there.
   ```json
   "pi": {
     "extensions": ["dist/extension.js"],
     "skills": ["skills/"],
     "prompts": ["prompts/"],
     "themes": ["themes/"]
   }
   ```

2. **Conventions**: If no manifest entry exists, Pi looks for standard directories:
   - `extensions/` (or `index.ts`/`index.js` for single-file extensions)
   - `skills/`
   - `prompts/`
   - `themes/`

## Configuration

You can manually configure packages in `settings.json`:

```json
{
  "packages": [
    "npm:pi-skills",
    {
      "source": "git:github.com/org/repo",
      "skills": ["relevant-skill"],
      "extensions": [] 
    }
  ]
}
```

The object form allows filtering which resources are loaded (allowlisting). If a field is omitted,
all resources of that type are loaded. If a field is present but empty, **no** resources of that
type are enabled.

### Filter Patterns

Filter values can be strings or arrays. They accept glob-like patterns relative to the package
root, with optional prefixes:

- `pattern` → include matches (if any include patterns exist)
- `!pattern` → exclude matches
- `+pattern` → **force include** (exact path match)
- `-pattern` → **force exclude** (exact path match)

Example:

```json
{
  "packages": [
    {
      "source": "git:github.com/org/repo",
      "extensions": ["dist/*.js", "!dist/experimental.js"],
      "skills": []
    }
  ]
}
```
