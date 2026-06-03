from pathlib import Path
t = Path('py/src/florecon/_host.py').read_text()
import re
t = re.sub(r'    def _call\(self, fn, payload: dict, arrow_bytes: bytes = None\) -> dict:\n[\s\S]+?(?=    def solve)', '''    def _call(self, fn, payload: dict, arrow_bytes: bytes = None) -> dict:
        data = json.dumps(payload).encode("utf-8")
        n = len(data)
        ptr = self._alloc(self.store, n)
        self.memory.write(self.store, data, ptr)
        
        arrow_n = len(arrow_bytes) if arrow_bytes else 0
        arrow_ptr = 0
        if arrow_n > 0:
            arrow_ptr = self._alloc(self.store, arrow_n)
            self.memory.write(self.store, arrow_bytes, arrow_ptr)
            
        packed = fn(self.store, ptr, n, arrow_ptr, arrow_n)
        
        self._dealloc(self.store, ptr, n)
        if arrow_n > 0:
            self._dealloc(self.store, arrow_ptr, arrow_n)
        
        out_ptr = packed & 0xFFFFFFFF
        out_len = (packed >> 32) & 0xFFFFFFFF
        out = self.memory.read(self.store, out_ptr, out_ptr + out_len)
        self._dealloc(self.store, out_ptr, out_len)
        return json.loads(bytes(out))

''', t)
Path('py/src/florecon/_host.py').write_text(t)
