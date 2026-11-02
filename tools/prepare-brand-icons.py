"""Package approved artwork: trim transparent padding, preserve its design, emit native sizes."""
from pathlib import Path
from PIL import Image
import struct

root = Path(__file__).resolve().parents[1]
sizes = (16, 20, 24, 32, 40, 48, 64, 96, 128, 256)

def pack_ico(image, path):
    # Supply each exact-size frame explicitly, rather than one 256px fallback.
    import io
    frames=[]
    for n in sizes:
        stream=io.BytesIO()
        image.resize((n,n),Image.Resampling.LANCZOS).save(stream,format='PNG')
        frames.append((n,stream.getvalue()))
    offset=6+16*len(frames);header=bytearray(struct.pack('<HHH',0,1,len(frames)))
    for n,data in frames:
        header.extend(struct.pack('<BBBBHHII',n%256,n%256,0,0,1,32,len(data),offset));offset+=len(data)
    path.write_bytes(header+b''.join(data for n,data in frames))

for theme, source in [('light', 'Copper Navigator App Icon.png'), ('dark', 'Copper Compass Emblem App Icon.png')]:
    image = Image.open(root / source).convert('RGBA')
    alpha=image.getchannel('A')
    # Ignore nearly transparent speckles outside the actual tile when measuring.
    box=alpha.point(lambda v:255 if v>=128 else 0).getbbox()
    image=image.crop(box)
    image.putalpha(image.getchannel('A').point(lambda v:255 if v>=250 else 0 if v<8 else v))
    side=max(image.size);canvas=Image.new('RGBA',(side,side))
    canvas.paste(image,((side-image.width)//2,(side-image.height)//2))
    # One-pixel breathing room at 32px, so the tile edge remains legible.
    art=canvas.resize((960,960),Image.Resampling.LANCZOS)
    image=Image.new('RGBA',(1024,1024));image.paste(art,(32,32))
    image.save(root / 'assets' / f'icon-{theme}.png')
    image.resize((512, 512), Image.Resampling.LANCZOS).save(root / 'ui' / f'brand-{theme}.png')
    image.resize((256, 256), Image.Resampling.LANCZOS).save(root / 'src-tauri/icons' / f'brand-{theme}.png')
    if theme == 'light':
        image.resize((1024, 1024), Image.Resampling.LANCZOS).save(root / 'assets/icon-source.png')
        image.resize((512, 512), Image.Resampling.LANCZOS).save(root / 'ui/brand-icon.png')
        for path in (root / 'src-tauri/icons').glob('*.png'):
            if path.name.startswith('brand-'): continue
            size = Image.open(path).size
            image.resize(size, Image.Resampling.LANCZOS).save(path)
        pack_ico(image,root / 'src-tauri/icons/icon.ico')
