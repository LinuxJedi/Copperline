; Packed, disjoint A->D and ABC->D throughput workload.
;
; Boots through the normal timing-test loader. Runs 2048 pairs of 20x256-word
; blits, then leaves a static bitmap on screen. BLTPRI is set; the CPU polls
; BBUSY and the ordinary chip-bus arbiter services all DMA. A and B are separate
; from D; C aliases D to exercise live destination reads. No clocks or emulator
; diagnostics are used by the guest. Benchmark 60 emulated seconds from boot
; so the final snapshot is idle, including on the slower AROS boot path.
CUST    equ $dff000
SRCA    equ $40000
SRCB    equ $44000
DEST    equ $48000
CLIST   equ $60000
WORDS   equ 20*256

        lea CUST,a6
        move.w #$7fff,$9a(a6)
        move.w #$7fff,$9c(a6)
        move.w #$7fff,$96(a6)
        lea SRCA,a0
        lea SRCB,a1
        lea DEST,a2
        move.w #WORDS-1,d0
        move.w #$5a3c,d1
.init:  move.w d1,(a0)+
        not.w d1
        move.w d1,(a1)+
        clr.w (a2)+
        ror.w #1,d1
        dbra d0,.init

        lea CLIST,a0
        move.l #$01001200,(a0)+  ; one low-resolution plane
        move.l #$01020000,(a0)+
        move.l #$01080000,(a0)+
        move.l #$010a0000,(a0)+
        move.l #$01800113,(a0)+
        move.l #$01820fff,(a0)+
        move.l #$00920038,(a0)+
        move.l #$009400d0,(a0)+
        move.l #$008e2c81,(a0)+
        move.l #$00902cc1,(a0)+
        move.l #$00e00004,(a0)+
        move.l #$00e28000,(a0)+  ; reset BPL1PT to DEST each frame
        move.l #$fffffffe,(a0)+
        move.l #CLIST,$80(a6)
        move.w #0,$88(a6)
        move.w #$87c0,$96(a6)
        move.w #$ffff,$44(a6)
        move.w #$ffff,$46(a6)
        clr.w $42(a6)
        clr.w $60(a6)
        clr.w $62(a6)
        clr.w $64(a6)
        clr.w $66(a6)

        move.w #2047,d7
.loop:  move.w #$09f0,$40(a6)
        move.l #SRCA,$50(a6)
        move.l #DEST,$54(a6)
        move.w #256*64+20,$58(a6)
.copy:  btst #6,$02(a6)
        bne.s .copy
        move.w #$0fca,$40(a6)
        move.l #SRCA,$50(a6)
        move.l #SRCB,$4c(a6)
        move.l #DEST,$48(a6)
        move.l #DEST,$54(a6)
        move.w #256*64+20,$58(a6)
.merge: btst #6,$02(a6)
        bne.s .merge
        dbra d7,.loop
.halt:  bra.s .halt
