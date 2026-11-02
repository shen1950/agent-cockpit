"""Create a reviewed-source-only ZIP, without .git, local backups or accounts."""
import argparse
from pathlib import Path
import zipfile
from privacy_check import ROOT, scan, source_files

parser = argparse.ArgumentParser()
parser.add_argument('output', type=Path)
args = parser.parse_args()
findings = scan()
if findings:
    raise SystemExit(f'Export stopped: {len(findings)} privacy findings. Run privacy_check.py.')
output = args.output.resolve()
if output.is_relative_to(ROOT):
    raise SystemExit('Choose an output location outside the repository.')
if output.exists():
    raise SystemExit('Output already exists; choose a new name.')
output.parent.mkdir(parents=True, exist_ok=True)
with zipfile.ZipFile(output, 'x', zipfile.ZIP_DEFLATED) as archive:
    for path in source_files():
        relative = path.relative_to(ROOT)
        if any(part.startswith('.') and part not in {'.cargo', '.gitignore', '.github'} for part in relative.parts):
            continue
        if relative.name.endswith(('.bak', '.log', '.pdb')):
            continue
        archive.write(path, Path('AgentCockpit') / relative)
print(f'Exported source snapshot: {output.name} (no Git history or ignored local data).')
