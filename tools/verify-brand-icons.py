"""Inspect the real Windows PE icon resources; do not trust Explorer's icon cache."""
import argparse
import ctypes as c
from ctypes import wintypes as w
from pathlib import Path
import struct

def ico_entries(path):
    data = Path(path).read_bytes()
    _, kind, count = struct.unpack_from('<HHH', data)
    assert kind == 1
    result = {}
    for i in range(count):
        width, height, _, _, _, _, size, offset = struct.unpack_from('<BBBBHHII', data, 6+i*16)
        result[(width or 256, height or 256)] = data[offset:offset+size]
    return result

def pe_icons(path):
    api = c.WinDLL('kernel32', use_last_error=True)
    api.LoadLibraryExW.argtypes = [w.LPCWSTR, w.HANDLE, w.DWORD]
    api.LoadLibraryExW.restype = w.HMODULE
    api.FindResourceW.argtypes = [w.HMODULE, c.c_void_p, c.c_void_p]
    api.FindResourceW.restype = w.HANDLE
    api.LoadResource.argtypes = [w.HMODULE, w.HANDLE]; api.LoadResource.restype = w.HANDLE
    api.SizeofResource.argtypes = [w.HMODULE, w.HANDLE]; api.SizeofResource.restype = w.DWORD
    api.LockResource.argtypes = [w.HANDLE]; api.LockResource.restype = c.c_void_p
    api.FreeLibrary.argtypes = [w.HMODULE]
    module = api.LoadLibraryExW(str(Path(path).resolve()), None, 0x22)
    if not module: raise c.WinError(c.get_last_error())
    def read(name, kind):
        resource = api.FindResourceW(module, name, kind)
        if not resource: raise c.WinError(c.get_last_error())
        return c.string_at(api.LockResource(api.LoadResource(module,resource)),api.SizeofResource(module,resource))
    groups = []
    callback_type = c.WINFUNCTYPE(w.BOOL, w.HMODULE, c.c_void_p, c.c_void_p, c.c_ssize_t)
    @callback_type
    def collect(module, kind, name, param):
        groups.append(read(name,14)); return True
    api.EnumResourceNamesW.argtypes = [w.HMODULE,c.c_void_p,callback_type,c.c_ssize_t]
    try:
        api.EnumResourceNamesW(module,14,collect,0)
        if not groups: raise ValueError('EXE has no icon group')
        result = {}
        for group in groups:
            for i in range(struct.unpack_from('<H',group,4)[0]):
                width,height,_,_,_,_,_,identifier=struct.unpack_from('<BBBBHHIH',group,6+i*14)
                result[(width or 256,height or 256)] = read(identifier,3)
        return result
    finally: api.FreeLibrary(module)

if __name__ == '__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--binary',required=True);parser.add_argument('--inspect',action='store_true')
    args=parser.parse_args(); expected=ico_entries('src-tauri/icons/icon.ico');actual=pe_icons(args.binary)
    print('PE icon sizes:', ', '.join(str(k[0]) for k in actual))
    same = all(actual.get(k)==v for k,v in expected.items())
    print('PE artwork matches current ICO:',same)
    if not args.inspect:
        assert same, 'Stale embedded icon resource'
        assert {16,20,24,32,40,48,64,128,256} <= {s[0] for s in actual}
        print('PASS compiled EXE contains the current multi-resolution Copper icon.')
