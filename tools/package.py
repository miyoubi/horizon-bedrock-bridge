#!/usr/bin/env python3
"""Package the mod as a source-only `.hnmod`, the file Horizon's registry and launcher take.

The archive holds `horizon-native.json` and `mixins/<id>/` (only `.rs` and `.toml`), with fixed dates and
modes, so the same sources always give the same bytes and the same SHA-256. Horizon's
`horizon_mods::index::native` reads it without running anything; the launcher compiles it on the pinned
Horizon revision. Based on levzzz5154's packager for Veinminer.
"""
import hashlib
import json
from pathlib import Path
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
GAME = '26.3'


def package():
    compat = json.loads((ROOT / 'compatibility.json').read_text())
    manifests = sorted((ROOT / 'mixins').glob('*/horizon-mixin.toml'))
    if len(manifests) != 1:
        raise SystemExit('a .hnmod holds one bundle: keep one mixins/<id>/horizon-mixin.toml')
    bundle = manifests[0].parent
    mixin = tomllib.loads(manifests[0].read_text())
    mod = mixin['mod']
    if bundle.name != mod['id']:
        raise SystemExit(f"rename mixins/{bundle.name}/ to mixins/{mod['id']}/ (the registry finds it by id)")
    if not mod['id'].replace('_', '').isalnum() or not mod['id'].islower():
        raise SystemExit('the registry takes ids of lowercase letters, digits and _ only')
    if mixin.get('compatibility', {}).get('horizon_revision') != compat['horizon_revision']:
        raise SystemExit('horizon-mixin.toml and compatibility.json pin different Horizon revisions')
    manifest = dict(schema=1, id=mod['id'], version=mod['version'], game=GAME,
                    horizon=compat['horizon_revision'], mixins=compat['horizon_mixins_revision'])
    text = (json.dumps(manifest, indent=2, sort_keys=True) + '\n').encode()
    # At the repo root too: a folder with a `horizon-mixin.toml` at the top is read as a plain
    # source mod and its pins are dropped (horizon_mixins::bundle_in passes `mixins: None`), so a
    # mod installed straight from the repository would get the launcher's built-in horizon-mixins.
    # The root manifest marks the repository as an unpacked `.hnmod`, which keeps the pins.
    (ROOT / 'horizon-native.json').write_bytes(text)
    files = {'horizon-native.json': text}
    # Recurse into the bundle: hooks live in hooks/ and the bundle crate in
    # engine/ (its Cargo.toml and src/*.rs), so a flat scan would drop them.
    for path in sorted(bundle.rglob('*')):
        if not path.is_file():
            continue
        relative = path.relative_to(bundle).as_posix()
        # What a bundle may hold: Rust and TOML anywhere, other files only under data/.
        if path.suffix not in ('.rs', '.toml') and not relative.startswith('data/'):
            continue
        if 'target' in path.relative_to(bundle).parts:
            continue
        content = path.read_bytes()
        files[path.relative_to(ROOT).as_posix()] = content
    out = ROOT / 'target' / f"{mod['id']}-{mod['version']}.hnmod"
    out.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
        for name, content in sorted(files.items()):
            info = zipfile.ZipInfo(name, (2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100644 << 16
            archive.writestr(info, content)
    print(json.dumps(dict(file=str(out), size=out.stat().st_size,
                          sha256=hashlib.sha256(out.read_bytes()).hexdigest()), indent=2))


if __name__ == '__main__':
    package()
