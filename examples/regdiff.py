#!/usr/bin/env python3
"""Harnais de diff registre SID : notre regdump vs sidplay --regdump, recalé.
Usage: regdiff.py <tune> <song0based> <secs>
Génère les 2 regdumps puis sort un tableau par registre (% match recalé +
valeurs en trop/manquantes). python r[k]=d(0x400+k-1). v_i ctrl=r[5+7i]."""
import csv, subprocess, sys, os
from collections import Counter

SIDDIR = os.path.expanduser('~/Documents/xm-rs/xmrs/src/tracker/import/sid/songs')
SIDPLAY = os.path.expanduser('~/Documents/sidera/target/release/sidplay')
SYNTH = os.path.expanduser('~/Documents/xm-rs/xmrsplayer/target/release/examples/sid_regdump_synth')
FILE = {  # tune -> .sid
 'commando':'commando.sid','monty':'monty_on_the_run.sid','crazy_comets':'crazy_comets.sid',
 'last_v8':'last_v8.sid','thing_on_a_spring':'thing_on_a_spring.sid','zoids':'zoids.sid',
 'delta':'delta.sid','sanxion':'sanxion.sid','lightforce':'lightforce.sid','spellbound':'spellbound.sid',
 'ace_2':'ace_2.sid','human_race':'human_race.sid','international_karate':'international_karate.sid',
 'thrust':'thrust.sid'}
NAMES={}
for i in range(3):
    b=1+7*i
    for off,nm in [(0,'lo'),(1,'hi'),(2,'pwl'),(3,'pwh'),(4,'ctrl'),(5,'AD'),(6,'SR')]:
        NAMES[b+off]='v%d%s'%(i,nm)
for k,nm in [(22,'FClo'),(23,'FChi'),(24,'resfilt'),(25,'modevol')]: NAMES[k]=nm

def load(fn):
    return list(csv.reader(open(fn)))[1:]

def main():
    tune,song,secs=sys.argv[1],int(sys.argv[2]),int(sys.argv[3])
    s_csv,o_csv='/tmp/rd_s.csv','/tmp/rd_o.csv'
    subprocess.run([SYNTH,tune,s_csv,str(secs),str(song)],stderr=subprocess.DEVNULL,check=True)
    subprocess.run([SIDPLAY,os.path.join(SIDDIR,FILE[tune]),'--song',str(song+1),
                    '--regdump',o_csv,'--seconds',str(secs)],stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,check=True)
    S,O=load(s_csv),load(o_csv); ns,no=len(S),len(O)
    # The sidplay --regdump captures the post-init (idle) state as frame 0 and
    # the first play() result as frame 1, while our synth dumps the first play()
    # as frame 0 → a uniform 1-frame capture offset on tunes that idle voice 0/2
    # at start. Detect it (compare the whole-row match at shift 0 vs 1) and align
    # the oracle so the metric reflects true register fidelity, not the offset.
    def ctrlmatch(sh):
        if ns==0 or no==0: return 0
        return sum(1 for i in range(ns) for k in (5,12,19)
                   if S[i][k]==O[min(no-1,int(i*no/ns)+sh)][k])
    # The replayer's init takes a tune-specific number of idle frames before the
    # music starts, which sidplay's --regdump captures as leading frames 0..k-1
    # while our synth starts at frame 0 → a uniform k-frame capture offset (not a
    # playback bug). Pick the offset that best aligns the control registers.
    oshift = max(range(0, 7), key=ctrlmatch) if ns==no else 0
    print('%s song%d : synth=%d oracle=%d frames (oracle shift=%d)'%(tune,song,ns,no,oshift))
    print('  reg      match%%  (recalé)   en_trop/manquant (distribution, count>10)')
    for k in sorted(NAMES):
        # match recalé frame-par-frame (avec l'offset de capture détecté)
        m=sum(1 for i in range(ns) if S[i][k]==O[min(no-1,int(i*no/ns)+oshift)][k])
        pct=100*m/ns if ns else 0
        # distribution
        co=Counter(r[k] for r in O); cs=Counter(r[k] for r in S)
        so={v for v,n in co.items() if n>10}; ss={v for v,n in cs.items() if n>10}
        extra=sorted(ss-so); miss=sorted(so-ss)
        # freq lo/hi : tolérer (détune + dérive) ; on ne flag que ctrl/PW/AD/SR/filtre
        isfreq = NAMES[k][-2:] in ('lo','hi') and NAMES[k] not in ('FClo','FChi')
        flag=''
        if not isfreq and (pct<88 or extra or miss):
            flag=' <<< trop=%s manq=%s'%(extra[:4],miss[:4])
        print('  %-8s %5.1f%%%s'%(NAMES[k],pct,flag))

if __name__=='__main__': main()
