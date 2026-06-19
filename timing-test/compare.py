#!/usr/bin/env python3
# Compare Copperline timing-test rows against the user's FS-UAE A500+/68EC020 reference.
import sys,subprocess,os
# FS-UAE reference (A500+, 68EC020 @7MHz, 2M chip, 512K slow, KS2.05, PAL)
REF=[0x2CF7,0x233A,0x2CF5,0x2339,0x2006,0x266D,0x7007,0x199F,0x377B,0x0B43,
     0x0470,0x0470,0x046F,0x19A6,0x19A4,0x0472,0x0C63,0x0C61,0x046F,0x0027,
     0x0111,0x0C56,0x0017,0x26B4,0x47B5,0x0106,0x6278]
DESC={0:"slowR",1:"slowW",2:"chipR",3:"chipW",4:"move",5:"shift",6:"mul",7:"dbra",
 8:"frame",9:"slowRd/f",10:"cw1024",11:"cw/6bpl",12:"cw/8spr",13:"dbraSlow",14:"dbraChip",
 15:"cw/6bpl+8spr",16:"cw/f",17:"cw/f+VB",18:"cw/3bpl",19:"VBentry",20:"SOFTend",
 21:"cw/chain",22:"VBraise",23:"blitClr",24:"blitFill",25:"blitLine",26:"fill+3bpl"}
# rows 19,20,22 are raw VHPOSR (vpos<<8 | hpos/2) not tick counts; ratio not meaningful
RAW={19,20,22}
def run():
    out=subprocess.run(["../target/release/copperline","--config","tt-fsuae-match.toml",
        "--noaudio","--screenshot-after","16","/tmp/tt_cmp.png"],capture_output=True,text=True,
        cwd="timing-test").stdout
    return [int(x,16) for x in out.replace('\0','').split() if len(x)==8 and all(c in '0123456789ABCDEFabcdef' for c in x)][:27]
rows=run()
if len(rows)<27:
    print(f"only {len(rows)} rows: {rows}"); sys.exit(1)
print(f"{'row':>3} {'desc':14} {'CL':>8} {'FS-UAE':>8} {'CL/FS':>6}  flag")
worst=[]
for i in range(27):
    r=REF[i]; c=rows[i]
    if i in RAW:
        print(f"{i:>3} {DESC[i]:14} {c:>8} {r:>8} {'(raw)':>6}  {'DIFF' if c!=r else 'ok'}")
        continue
    ratio=c/r if r else 0
    off=abs(ratio-1)
    flag="<<<" if off>0.15 else ("<<" if off>0.06 else "")
    if off>0.06 and i not in (8,): worst.append((off,i))
    print(f"{i:>3} {DESC[i]:14} {c:>8} {r:>8} {ratio:>6.2f}  {flag}")
worst.sort(reverse=True)
print("worst:", [f"r{i}({DESC[i]} {o*100:.0f}%)" for o,i in worst[:10]])
