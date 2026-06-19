; Copperline/vAmiga/FS-UAE cross-emulator CPU & bus timing test.
;
; Loaded by boot.asm to $30000, this program takes over the machine (interrupts
; and DMA off), runs a battery of timing tests measured with the CIA-A timer A
; (E-clock, = CPU clock / 10), and renders the raw results on screen as 8-digit
; hex numbers (also streamed out the serial port), one test per row.
;
; Because the only clock is the CIA E-clock, every emulator that models the
; CPU-cycle / E-clock ratio correctly should report identical numbers. A
; mismatch localises exactly which operation an emulator times differently.
;
; Rows (each = elapsed E-clock ticks; rows 0-7 are 8192 iterations):
;   row 0  slow-RAM read    move.w (a0),d0    a0=$C00000
;   row 1  slow-RAM write   move.w d1,(a0)    a0=$C00000
;   row 2  chip-RAM read    move.w (a0),d0    a0=$060000
;   row 3  chip-RAM write   move.w d1,(a0)    a0=$060000
;   row 4  reg move         move.w d2,d0
;   row 5  shift            lsl.l #8,d3
;   row 6  multiply         mulu #$5555,d5
;   row 7  loop baseline    dbra only (code in chip RAM at $30000)
;   row 8  frame length     E-clock ticks for one whole video frame
;   row 9  slow reads/frame  slow reads from frame top to vpos 280
;   row 10 chip write x1024  no display DMA
;   row 11 chip write x1024  during a 6-bitplane lores display (contention)
;   row 12 chip write x1024  during 8-sprite DMA (contention)
;   row 13 dbra x8192        code executed from SLOW RAM ($C00000)
;   row 14 dbra x8192        code executed from chip RAM ($060000)
;   row 15 chip write x1024  during 6-bitplane AND 8-sprite DMA (combined)
;   row 16 chip writes/frame no interrupts (per-frame throughput baseline)
;   row 17 chip writes/frame with a VERTB interrupt each frame (16-17 = irq cost)
;   row 18 chip write x1024  during 3-bitplane lores DMA (plane-count scaling)
;   row 19 VHPOSR at VERTB-handler entry (chained interrupt latency from vblank)
;   row 20 VHPOSR at end of the chained SOFTINT task switch (20-19 = chain cost)
;   row 21 chip writes/frame with VERTB+SOFTINT+task-switch each frame (full chain)
;   row 22 VHPOSR when INTREQR VERTB bit sets (pure raise position, no exception)
;   row 23 BEAM cck advanced while a D-only clear blit (220x22) runs (display off)
;   row 24 BEAM cck advanced while an A->D fill blit (272x22) runs (display off)
;   row 25 BEAM cck advanced while a 64-pixel line blit runs (display off)
;   row 26 BEAM cck advanced while the A->D fill (as 24) runs WITH a 3-bitplane
;          display active and BLTPRI set (26-24 = display contention on the
;          blitter; this is the active-display beam-vs-blitter race condition)
;   row 27 VHPOSR when the CPU first sees a COPPER-raised INTREQR.COPER (copper
;          waits for line $64 then MOVEs SET-COPER into INTREQ; CPU polls with
;          interrupts off). The copper-vs-CPU phase the 020 chase-the-beam
;          copper-chunky effects depend on. FFFFFFFF if the copper never fired.
;
; Rows 0/1/9/13 use slow RAM ($C00000); on a machine without it (A1200) they
; store a 0 sentinel. All other rows -- including 27 -- run regardless.
;
; Rows 23-26 measure blitter cycle timing in raster (beam) units, the dimension
; the CPU/contention rows above cannot see: whether a beam-raced blitter draw
; keeps ahead of the display fetch. They use vpos*227+hpos as the beam clock.
;
; Scratch chip-RAM addresses: $40000 screen (1 plane 320x256), $48000 results,
; $60000 chip-RAM test target, $20000 sprite/bitplane DMA buffer. The program is
; position-independent (PC-relative + fixed scratch addresses).

CUST    equ     $dff000
ITERS   equ     $2000           ; 8192 iterations per fixed-count test
SCREEN  equ     $40000
RESULTS equ     $48000
CHIPT   equ     $60000
SLOWT   equ     $c00000
; small RAM scratch for the interrupt-chain rows (chip RAM, above RESULTS):
CHAINEN equ     $4f000          ; beam (VHPOSR) at VERTB-handler entry
CHAINND equ     $4f004          ; beam (VHPOSR) at the SOFTINT task-switch end
SAVESP  equ     $4f008          ; SP save slot for the mimic task switch
SLOWPRES equ    $4f00c          ; word: 1 if slow RAM ($C00000) is present, else 0
COPLIST equ     $50000          ; scratch copper list for the copper-phase row (27)

; This is the main program. It is loaded to $30000 by boot.asm (the boot block)
; and entered here. The code is position-independent (PC-relative + fixed scratch
; addresses) so the load address does not matter.
;----------------------------------------------------- entry (a6=sys at load)
boot:
        lea     CUST,a6
        move.w  #$7fff,$9a(a6)  ; INTENA: disable all interrupts
        move.w  #$7fff,$9c(a6)  ; INTREQ: clear all pending
        move.w  #$7fff,$96(a6)  ; DMACON: disable all DMA
        ; INTENA is fully cleared above, so no interrupt can reach the CPU; we
        ; stay in user mode (the boot block is entered unprivileged) and avoid
        ; the privileged SR write entirely.
        move.w  #$0f00,$180(a6) ; "alive" border colour (red) until display is up

        ; clear the screen bitplane
        lea     SCREEN,a0
        move.w  #(40*256/4)-1,d0
.clrs   clr.l   (a0)+
        dbra    d0,.clrs

        lea     RESULTS,a3      ; result write pointer

        ; Probe slow RAM ($C00000). It is absent on the A1200 (no trapdoor RAM
        ; there), so the slow-RAM rows (0,1,9,13) would otherwise read or -- for
        ; row 13's jsr -- EXECUTE unmapped space and trap. Write two distinct
        ; words and read them back: a real RAM keeps both, a floating bus cannot
        ; match both. When absent those rows store a 0 sentinel instead.
        move.w  #$a5a5,SLOWT
        move.w  #$5a5a,SLOWT+2
        moveq   #0,d0
        cmp.w   #$a5a5,SLOWT
        bne     .noslow
        cmp.w   #$5a5a,SLOWT+2
        bne     .noslow
        moveq   #1,d0
.noslow move.w  d0,SLOWPRES

        ; row 0: slow-RAM read
        tst.w   SLOWPRES
        beq     .r0skip
        lea     SLOWT,a0
        bsr     tstart
        move.w  #ITERS-1,d6
.t0     move.w  (a0),d0
        dbra    d6,.t0
        bsr     tread
        move.l  d0,(a3)+
        bra     .r0done
.r0skip clr.l   (a3)+
.r0done

        ; row 1: slow-RAM write
        tst.w   SLOWPRES
        beq     .r1skip
        lea     SLOWT,a0
        bsr     tstart
        move.w  #ITERS-1,d6
.t1     move.w  d1,(a0)
        dbra    d6,.t1
        bsr     tread
        move.l  d0,(a3)+
        bra     .r1done
.r1skip clr.l   (a3)+
.r1done

        ; row 2: chip-RAM read
        lea     CHIPT,a0
        bsr     tstart
        move.w  #ITERS-1,d6
.t2     move.w  (a0),d0
        dbra    d6,.t2
        bsr     tread
        move.l  d0,(a3)+

        ; row 3: chip-RAM write
        lea     CHIPT,a0
        bsr     tstart
        move.w  #ITERS-1,d6
.t3     move.w  d1,(a0)
        dbra    d6,.t3
        bsr     tread
        move.l  d0,(a3)+

        ; row 4: register move
        bsr     tstart
        move.w  #ITERS-1,d6
.t4     move.w  d2,d0
        dbra    d6,.t4
        bsr     tread
        move.l  d0,(a3)+

        ; row 5: shift lsl.l #8
        bsr     tstart
        move.w  #ITERS-1,d6
.t5     lsl.l   #8,d3
        dbra    d6,.t5
        bsr     tread
        move.l  d0,(a3)+

        ; row 6: mulu #$5555 (source bit count fixed -> fixed cycle count)
        bsr     tstart
        move.w  #ITERS-1,d6
.t6     mulu    #$5555,d5
        dbra    d6,.t6
        bsr     tread
        move.l  d0,(a3)+

        ; row 7: empty dbra loop baseline
        bsr     tstart
        move.w  #ITERS-1,d6
.t7     dbra    d6,.t7
        bsr     tread
        move.l  d0,(a3)+

        move.w  #$00f0,$180(a6) ; phase marker: fixed tests done (green)

        ; row 8: frame length in E-clock ticks (one full frame wrap to wrap)
        bsr     syncframe
        bsr     tstart
        bsr     syncframe
        bsr     tread
        move.l  d0,(a3)+
        move.w  #$000f,$180(a6) ; phase marker: frame-length test done (blue)

        ; row 9: slow-RAM reads from frame top until beam reaches vpos 280
        tst.w   SLOWPRES
        beq     .r9skip
        bsr     syncframe       ; sync to a frame start (vpos near 0)
        moveq   #0,d7
        lea     SLOWT,a0
.t9b    move.w  (a0),d0         ; the measured work
        addq.l  #1,d7
        move.w  $004(a6),d1     ; build vpos
        and.w   #1,d1
        lsl.w   #8,d1
        move.w  $006(a6),d2
        lsr.w   #8,d2
        or.w    d2,d1
        cmp.w   #280,d1
        blo     .t9b            ; keep counting until the beam nears frame bottom
        move.l  d7,(a3)+
        bra     .r9done
.r9skip clr.l   (a3)+
.r9done

        ; row 10: chip-RAM write, N=1024, NO display DMA (contention baseline)
        lea     CHIPT,a0
        bsr     tstart
        move.w  #1024-1,d6
.t10    move.w  d1,(a0)
        dbra    d6,.t10
        bsr     tread
        move.l  d0,(a3)+

        ; row 11: chip-RAM write, N=1024, WHILE 6-bitplane lores DMA is active.
        ; This is the heavy-display case: the CPU writes chip RAM in the same slots
        ; the display fetcher wants, so the difference (row 11 - row 10) is the
        ; per-emulator chip-bus contention cost during a heavy display.
        move.w  #$6000,$100(a6) ; BPLCON0: 6 bitplanes, lores
        move.w  #$0038,$092(a6) ; DDFSTRT (full lores width)
        move.w  #$00d0,$094(a6) ; DDFSTOP
        move.w  #$2c81,$08e(a6) ; DIWSTRT
        move.w  #$2cc1,$090(a6) ; DIWSTOP
        move.w  #$0000,$108(a6) ; BPL1MOD
        move.w  #$0000,$10a(a6) ; BPL2MOD
        move.l  #$00020000,d0   ; point all 6 bitplanes at a scratch buffer
        move.w  d0,$0e2(a6)
        move.w  d0,$0e6(a6)
        move.w  d0,$0ea(a6)
        move.w  d0,$0ee(a6)
        move.w  d0,$0f2(a6)
        move.w  d0,$0f6(a6)
        swap    d0
        move.w  d0,$0e0(a6)
        move.w  d0,$0e4(a6)
        move.w  d0,$0e8(a6)
        move.w  d0,$0ec(a6)
        move.w  d0,$0f0(a6)
        move.w  d0,$0f4(a6)
        move.w  #$8300,$096(a6) ; DMAEN | BPLEN
        bsr     syncframe       ; frame top
.c11    bsr     getvpos
        cmp.w   #60,d0
        blo     .c11            ; advance into the active display region
        lea     CHIPT,a0
        bsr     tstart
        move.w  #1024-1,d6
.t11    move.w  d1,(a0)
        dbra    d6,.t11
        bsr     tread
        move.w  #$0300,$096(a6) ; disable bitplane DMA again
        move.l  d0,(a3)+

        ; row 12: chip-RAM write, N=1024, during 8-sprite DMA. All 8 sprites share
        ; one data block (VSTART=$50, VSTOP=$B4) so sprite DMA fetches 16 words per
        ; line in the sprite region. The
        ; difference vs row 10 is the per-emulator sprite-DMA contention cost.
        lea     $0120(a6),a2    ; SPR0PT..SPR7PT live at $DFF120..$DFF13E
        lea     spritedata(pc),a1
        move.l  a1,d0
        moveq   #8-1,d4
.spp    move.l  d0,(a2)+        ; point all 8 sprites at the shared data block
        dbra    d4,.spp
        move.w  #$8220,$096(a6) ; DMAEN | SPREN
        bsr     syncframe
.c12    bsr     getvpos
        cmp.w   #$58,d0
        blo     .c12            ; advance into the sprites' active vertical range
        lea     CHIPT,a0
        bsr     tstart
        move.w  #1024-1,d6
.t12    move.w  d1,(a0)
        dbra    d6,.t12
        bsr     tread
        move.w  #$7fff,$096(a6) ; all DMA off again
        move.l  d0,(a3)+

        ; rows 0-12 all FETCH their code from chip RAM. Some workloads run from
        ; slow RAM, so measure the same dbra loop fetched from slow RAM (row 13)
        ; and from a second chip address (row 14). With DMA off both regions are
        ; 2 cck/word with no contention, so on real hardware rows 7, 13 and 14
        ; should all match; a difference is an Copperline code-location timing bug.
        ; row 13: loop executed from slow RAM ($C00000) -- skip if no slow RAM
        ; (jsr to unmapped space would execute floating-bus garbage and trap).
        tst.w   SLOWPRES
        beq     .r13skip
        lea     SLOWT,a1
        bsr     copytmpl
        bsr     tstart
        jsr     SLOWT
        bsr     tread
        move.l  d0,(a3)+
        bra     .r13done
.r13skip clr.l  (a3)+
.r13done

        lea     CHIPT,a1        ; row 14: loop executed from chip RAM ($060000)
        bsr     copytmpl
        bsr     tstart
        jsr     CHIPT
        bsr     tread
        move.l  d0,(a3)+

        ; row 15: chip-RAM write, N=1024, during 6-bitplane AND 8-sprite DMA at
        ; once. Compared with row 10 (no DMA), the ratio is the COMBINED
        ; contention; if it differs from real hardware, the integrated contention
        ; model is wrong.
        move.w  #$6000,$100(a6) ; BPLCON0: 6 bitplanes lores
        move.w  #$0038,$092(a6)
        move.w  #$00d0,$094(a6)
        move.w  #$2c81,$08e(a6)
        move.w  #$2cc1,$090(a6)
        move.w  #$0000,$108(a6)
        move.w  #$0000,$10a(a6)
        move.l  #$00020000,d0   ; 6 bitplane pointers -> scratch
        move.w  d0,$0e2(a6)
        move.w  d0,$0e6(a6)
        move.w  d0,$0ea(a6)
        move.w  d0,$0ee(a6)
        move.w  d0,$0f2(a6)
        move.w  d0,$0f6(a6)
        swap    d0
        move.w  d0,$0e0(a6)
        move.w  d0,$0e4(a6)
        move.w  d0,$0e8(a6)
        move.w  d0,$0ec(a6)
        move.w  d0,$0f0(a6)
        move.w  d0,$0f4(a6)
        lea     $0120(a6),a2    ; 8 sprite pointers -> shared sprite data
        lea     spritedata(pc),a1
        move.l  a1,d0
        moveq   #8-1,d4
.sp15   move.l  d0,(a2)+
        dbra    d4,.sp15
        move.w  #$8320,$096(a6) ; DMAEN | BPLEN | SPREN
        bsr     syncframe
.c15    bsr     getvpos
        cmp.w   #60,d0
        blo     .c15
        lea     CHIPT,a0
        bsr     tstart
        move.w  #1024-1,d6
.t15    move.w  d1,(a0)
        dbra    d6,.t15
        bsr     tread
        move.w  #$7fff,$096(a6) ; all DMA off
        move.l  d0,(a3)+

        ; row 16: chip writes from frame top to vpos 280, NO interrupts (the
        ; per-frame CPU-throughput baseline for the interrupt test below).
        move.w  #$7fff,$09a(a6) ; INTENA off
        bsr     syncframe
        moveq   #0,d7
        lea     CHIPT,a0
.t16    move.w  d1,(a0)
        addq.l  #1,d7
        move.w  $004(a6),d2
        and.w   #1,d2
        lsl.w   #8,d2
        move.w  $006(a6),d3
        lsr.w   #8,d3
        or.w    d3,d2
        cmp.w   #280,d2
        blo     .t16
        move.l  d7,(a3)+

        ; row 17: same, but a VERTB interrupt fires each frame. row 16 - row 17 is
        ; the per-frame interrupt entry+handler+exit cost. Multi-interrupt frames
        ; amplify this, so if an emulator's interrupt cost differs from real hardware
        ; this is where a frame-budget-tight demo would feel it.
        lea     vertb_handler(pc),a0
        move.l  a0,$6c.w        ; level-3 autovector (VERTB)
        move.w  #$0020,$09c(a6) ; clear any pending VERTB
        move.w  #$c020,$09a(a6) ; INTENA: master + VERTB enable
        bsr     syncframe
        moveq   #0,d7
        lea     CHIPT,a0
.t17    move.w  d1,(a0)
        addq.l  #1,d7
        move.w  $004(a6),d2
        and.w   #1,d2
        lsl.w   #8,d2
        move.w  $006(a6),d3
        lsr.w   #8,d3
        or.w    d3,d2
        cmp.w   #280,d2
        blo     .t17
        move.w  #$7fff,$09a(a6) ; INTENA off
        move.l  d7,(a3)+

        ; row 18: chip-RAM write, N=1024, during 3-bitplane lores DMA. Same as
        ; row 11 but 3 planes instead of 6. If Copperline's row-11 over-contention is
        ; a real per-line bitplane-DMA-edge bug it should scale with plane count
        ; (row 18 - row 10 ~= half of row 11 - row 10); if row 18 matches real
        ; hardware while row 11 does not, row 11 was a 6-plane phase artifact.
        move.w  #$3000,$100(a6) ; BPLCON0: 3 bitplanes, lores
        move.w  #$0038,$092(a6)
        move.w  #$00d0,$094(a6)
        move.w  #$2c81,$08e(a6)
        move.w  #$2cc1,$090(a6)
        move.w  #$0000,$108(a6)
        move.w  #$0000,$10a(a6)
        move.l  #$00020000,d0
        move.w  d0,$0e2(a6)
        move.w  d0,$0e6(a6)
        move.w  d0,$0ea(a6)
        swap    d0
        move.w  d0,$0e0(a6)
        move.w  d0,$0e4(a6)
        move.w  d0,$0e8(a6)
        move.w  #$8300,$096(a6) ; DMAEN | BPLEN
        bsr     syncframe
.c18    bsr     getvpos
        cmp.w   #60,d0
        blo     .c18
        lea     CHIPT,a0
        bsr     tstart
        move.w  #1024-1,d6
.t18    move.w  d1,(a0)
        dbra    d6,.t18
        bsr     tread
        move.w  #$7fff,$096(a6) ; all DMA off
        move.l  d0,(a3)+

        ; ----------------------------------------------------------------------
        ; rows 19-21: cooperative-scheduler interrupt chain. This measures the
        ; per-frame chain VERTB (level 3) -> SET SOFT
        ; -> level-1 SOFTINT -> cooperative TASK SWITCH (save regs, swap SP,
        ; restore regs) -> resume. Rows 16/17 only measured a bare VERTB ack;
        ; this chain (the SOFTINT + register-file task switch) is what those rows
        ; do NOT cover, and it is exactly where a beam-vs-CPU coupling difference
        ; can push a frame loop top a few lines later and trip its frame skip.
        ;
        ; The level-1 SOFT handler does the SAME work as a switcher (movem of
        ; all 15 data/address registers out and back, plus two longword SP moves
        ; that mimic loading the other task's stack pointer) but returns to the
        ; same context, so its CYCLE COST matches a real switch without needing a
        ; second task. The handlers each record the live beam position (VHPOSR:
        ; high byte = vpos, low byte = hpos/2) so we can see, in raster terms,
        ; where the chain starts and ends.
        lea     vchain_handler(pc),a0
        move.l  a0,$6c.w        ; level-3 autovector (VERTB)
        lea     soft_handler(pc),a0
        move.l  a0,$64.w        ; level-1 autovector (SOFT/DSKBLK/TBE)
        move.w  #$7fff,$09c(a6) ; clear all pending
        ; row 19: beam (VHPOSR) at VERTB-handler entry = interrupt latency from
        ;         the vblank. row 20: beam at the END of the chained SOFTINT task
        ;         switch. (row20 - row19) is the beam advance the whole VERTB ->
        ;         SET SOFT -> SOFT -> task-switch chain consumes -- the direct
        ;         "how many raster lines does this chain burn" number.
        clr.l   CHAINEN
        clr.l   CHAINND
        move.w  #$c024,$09a(a6) ; INTENA: master + VERTB($0020) + SOFT($0004)
        bsr     syncframe       ; let one full chain fire during the frame
        bsr     syncframe       ; and a second, so both vars are populated
        move.w  #$7fff,$09a(a6) ; INTENA off
        move.l  CHAINEN,(a3)+   ; row 19
        move.l  CHAINND,(a3)+   ; row 20

        ; row 21: chip writes from frame top to vpos 280 WHILE the full VERTB +
        ; SOFTINT + task-switch chain fires each frame (mirror of rows 16/17).
        ; row16 - row21 = the whole per-frame chain cost in displaced writes;
        ; row17 - row21 = the SOFTINT + task-switch increment over a bare VERTB.
        move.w  #$7fff,$09c(a6) ; clear pending
        move.w  #$c024,$09a(a6) ; master + VERTB + SOFT
        bsr     syncframe
        moveq   #0,d7
        lea     CHIPT,a0
.t21    move.w  d1,(a0)
        addq.l  #1,d7
        move.w  $004(a6),d2
        and.w   #1,d2
        lsl.w   #8,d2
        move.w  $006(a6),d3
        lsr.w   #8,d3
        or.w    d3,d2
        cmp.w   #280,d2
        blo     .t21
        move.w  #$7fff,$09a(a6) ; INTENA off
        move.l  d7,(a3)+

        ; row 22: beam (VHPOSR) when the INTREQR VERTB bit is first observed SET,
        ; interrupts OFF -- the PURE raise position, with no 68000 exception
        ; latency. Rows 19/20 measure handler ENTRY (raise + exception); row 22
        ; isolates the raise. If row 22 differs across emulators like row 19, the
        ; ~70cck discrepancy is a VERTB-raise-timing bug; if row 22 matches but
        ; row 19 does not, it is 68000 interrupt latency instead.
        clr.l   CHAINEN
        move.w  #$7fff,$09a(a6) ; INTENA off (poll only, no exception)
        bsr     syncframe       ; just past the top (VERTB freshly set)
        move.w  #$0020,$09c(a6) ; clear VERTB
.w22a   bsr     getvpos
        cmp.w   #200,d0
        blo     .w22a           ; advance well clear of the vblank
        move.w  #$0020,$09c(a6) ; clear again (ignore any spurious set)
.w22    move.w  $01e(a6),d0     ; INTREQR
        btst    #5,d0           ; VERTB pending yet?
        beq     .w22            ; spin until it sets
        ; Read the beam AFTER detecting the set, not before: a VHPOSR read in the
        ; first 1-2 hpos of a line returns the PREVIOUS line's vpos (a real Amiga
        ; VPOSR/VHPOSR line-start read delay). At the frame wrap that previous
        ; vpos is the last line of the frame, so a read latched right before the
        ; set could report ~vpos 312 (=56 in the VHPOSR byte). Reading once the
        ; VERTB bit is already up puts the beam a few cck into line 0, clear of
        ; that window, so the vpos reads as 0 on every emulator.
        move.w  $006(a6),CHAINEN+2 ; beam at the raise (post-detection)
        move.l  CHAINEN,(a3)+   ; row 22

        ; rows 23-25: BLITTER vs BEAM. None of the rows above run an active
        ; blitter, yet a beam-raced blitter draw (the blitter filling a buffer
        ; the display is simultaneously fetching) is exactly what decides
        ; whether a triple-buffered vector effect shows or blanks: if the blitter
        ; falls behind the display
        ; beam the fetched buffer line is still undrawn. Each row records how
        ; far the BEAM advances (in color clocks, vpos*227+hpos) while a single
        ; fixed blit of the type the effect uses runs to completion (polling
        ; DMACONR BBUSY). Display DMA is OFF so this is the pure blitter cycle
        ; cost against the beam; a faithful emulator should report the same cck
        ; on every emulator (the blitter and the beam share the chip clock). A
        ; difference here is a blitter-timing-vs-beam bug -- the dimension the
        ; CPU/contention rows above cannot see.
        move.w  #$8240,$096(a6) ; DMACON: DMAEN | BLTEN (display/sprite DMA off)
        move.w  #$ffff,$044(a6) ; BLTAFWM
        move.w  #$ffff,$046(a6) ; BLTALWM

        ; row 23: D-only clear (BLTCON0=$0100), 220 x 22 words -- per-frame
        ; back-buffer clear. HRM "- D" per word = 2 cck/word.
        bsr     syncframe
        move.w  #$0100,$040(a6) ; BLTCON0: USE D, LF=0 (clear)
        move.w  #$0000,$042(a6) ; BLTCON1
        move.w  #$0000,$066(a6) ; BLTDMOD
        move.l  #$20000,d0
        move.w  d0,$056(a6)     ; BLTDPT lo
        swap    d0
        move.w  d0,$054(a6)     ; BLTDPT hi
        bsr     getbeam
        move.l  d0,d7
        move.w  #(220<<6)|22,$058(a6) ; BLTSIZE -> start blit
        bsr     waitblit
        bsr     getbeam
        sub.l   d7,d0
        move.l  d0,(a3)+        ; row 23

        ; row 24: A->D copy with exclusive-fill, descending (BLTCON0=$09F0,
        ; BLTCON1=$0012), 272 x 22 -- per-frame polygon area fill.
        bsr     syncframe
        move.w  #$09f0,$040(a6) ; BLTCON0: USE A,D  LF=$F0 (D=A)
        move.w  #$0012,$042(a6) ; BLTCON1: DESC + exclusive fill
        move.w  #$0000,$064(a6) ; BLTAMOD
        move.w  #$0000,$066(a6) ; BLTDMOD
        move.w  #$ffff,$074(a6) ; BLTADAT (unused; A from memory)
        move.l  #$34000,d0      ; A pointer (descends, stays in chip RAM)
        move.w  d0,$052(a6)
        swap    d0
        move.w  d0,$050(a6)
        move.l  #$2c000,d0      ; D pointer
        move.w  d0,$056(a6)
        swap    d0
        move.w  d0,$054(a6)
        bsr     getbeam
        move.l  d0,d7
        move.w  #(272<<6)|22,$058(a6) ; BLTSIZE -> start blit
        bsr     waitblit
        bsr     getbeam
        sub.l   d7,d0
        move.l  d0,(a3)+        ; row 24

        ; row 25: a single LINE blit (BLTCON0=$bb4a, BLTCON1=$0003), 64 pixels
        ; -- polygon edge workloads can issue many of these. Line mode is
        ; 4 cck/pixel (HRM "- C - D"); the data is irrelevant, only the cadence.
        bsr     syncframe
        move.w  #$bb4a,$040(a6) ; BLTCON0: line, USE A,C,D
        move.w  #$0003,$042(a6) ; BLTCON1: LINE + octant bit
        move.w  #$ffff,$072(a6) ; BLTBDAT: solid texture
        move.w  #$0000,$062(a6) ; BLTBMOD (4*(dy-dx))
        move.w  #$0040,$064(a6) ; BLTAMOD (4*dy)
        move.w  #$0028,$060(a6) ; BLTCMOD (row stride)
        move.w  #$0000,$052(a6) ; BLTAPT lo (Bresenham accumulator)
        move.w  #$0000,$050(a6) ; BLTAPT hi
        move.l  #$26000,d0      ; C and D both point at the line dest
        move.w  d0,$04a(a6)     ; BLTCPT lo
        move.w  d0,$056(a6)     ; BLTDPT lo
        swap    d0
        move.w  d0,$048(a6)     ; BLTCPT hi
        move.w  d0,$054(a6)     ; BLTDPT hi
        bsr     getbeam
        move.l  d0,d7
        move.w  #(64<<6)|2,$058(a6) ; BLTSIZE: 64 pixels, width 2 -> start
        bsr     waitblit
        bsr     getbeam
        sub.l   d7,d0
        move.l  d0,(a3)+        ; row 25

        ; row 26: the A->D area fill (as row 24) but with a 3-bitplane lores
        ; display ACTIVE and BLTPRI set -- active-display contention condition.
        ; Bitplane DMA always outranks the blitter, so the fill takes longer in
        ; beam terms than row 24; row26 - row24 is the display-contention cost on
        ; the blitter. This is the exact "does the blitter keep ahead of the beam
        ; while the screen is fetching" figure that decides the spin's blank.
        move.w  #$3000,$100(a6) ; BPLCON0: 3 bitplanes, lores
        move.w  #$0038,$092(a6) ; DDFSTRT
        move.w  #$00d0,$094(a6) ; DDFSTOP
        move.w  #$2c81,$08e(a6) ; DIWSTRT
        move.w  #$2cc1,$090(a6) ; DIWSTOP
        move.w  #$0000,$108(a6) ; BPL1MOD
        move.w  #$0000,$10a(a6) ; BPL2MOD
        move.l  #$10000,d0      ; point the 3 display planes at a scratch buffer
        move.w  d0,$0e2(a6)
        move.w  d0,$0e6(a6)
        move.w  d0,$0ea(a6)
        swap    d0
        move.w  d0,$0e0(a6)
        move.w  d0,$0e4(a6)
        move.w  d0,$0e8(a6)
        move.w  #$8740,$096(a6) ; DMAEN | BPLEN | BLTEN | BLTPRI
        bsr     syncframe
        move.w  #$09f0,$040(a6) ; BLTCON0: USE A,D
        move.w  #$0012,$042(a6) ; BLTCON1: DESC + exclusive fill
        move.w  #$0000,$064(a6) ; BLTAMOD
        move.w  #$0000,$066(a6) ; BLTDMOD
        move.l  #$34000,d0      ; A pointer
        move.w  d0,$052(a6)
        swap    d0
        move.w  d0,$050(a6)
        move.l  #$2c000,d0      ; D pointer
        move.w  d0,$056(a6)
        swap    d0
        move.w  d0,$054(a6)
        ; start the fill a few lines into the active display so it overlaps the
        ; bitplane fetch, then time it to completion.
.c26    bsr     getvpos
        cmp.w   #60,d0
        blo     .c26
        bsr     getbeam
        move.l  d0,d7
        move.w  #(272<<6)|22,$058(a6) ; BLTSIZE -> start blit
        bsr     waitblit
        bsr     getbeam
        sub.l   d7,d0
        move.l  d0,(a3)+        ; row 26
        move.w  #$7fff,$096(a6) ; all DMA off again

        ; row 27: COPPER vs CPU interrupt phase. A copper list waits for line
        ; $64 then MOVEs SET-COPER into INTREQ; the CPU (interrupts off) busy-
        ; polls INTREQR for the COPER bit and records the beam (VHPOSR, high byte
        ; vpos / low byte hpos/2). This is the SANITY Roots II copper-chunky
        ; mechanism -- a copper-raised interrupt the per-frame work spins on -- so
        ; the raise+catch beam position is exactly what decides whether that
        ; effect renders or scrambles. A difference across emulators is a
        ; copper-WAIT-release / MOVE / poll-catch phase divergence.
        move.w  #$7fff,$096(a6) ; all DMA off
        move.w  #$7fff,$09a(a6) ; INTENA off (poll, no exception)
        move.w  #$7fff,$09c(a6) ; clear all pending
        lea     copperlist(pc),a0
        move.l  a0,d0
        move.w  d0,$082(a6)     ; COP1LCL (low word) -- NOT $084 (that is COP2LCH)
        swap    d0
        move.w  d0,$080(a6)     ; COP1LCH (high word)
        bsr     syncframe       ; sync to frame top with the copper DMA still off
        move.w  #$0010,$09c(a6) ; clear COPER before the copper raises it
        move.w  #$8280,$096(a6) ; DMACON: DMAEN | COPEN
        move.w  #$0000,$088(a6) ; COPJMP1: start the copper at COP1LC (beam ~line 0)
        move.l  #$100000,d1     ; timeout guard (so a non-firing copper cannot hang)
.w27    move.w  $01e(a6),d0     ; INTREQR
        btst    #4,d0           ; COPER raised yet?
        bne     .w27ok          ; caught it
        subq.l  #1,d1
        bne     .w27            ; spin until the copper sets it (or timeout)
        moveq   #-1,d0          ; never fired -> $FFFFFFFF sentinel
        move.l  d0,CHAINEN
        bra     .w27st
.w27ok  clr.w   CHAINEN
        move.w  $006(a6),CHAINEN+2 ; beam at the copper-IRQ catch
.w27st  move.l  CHAINEN,(a3)+   ; row 27
        move.w  #$7fff,$096(a6) ; all DMA off
        move.w  #$7fff,$09c(a6) ; clear pending

        move.w  #$0ff0,$180(a6) ; phase marker: all tests done (yellow)

        ;------------------------------------------------ render + show
        bsr     render

        ; Also stream the results out the serial port as ASCII hex (one 8-digit
        ; value per line). Copperline funnels this to stdout; FS-UAE/vAmiga can log
        ; the serial port to a file, giving a screenshot-free way to compare.
        move.w  #$0170,$032(a6) ; SERPER ~9600 baud
        lea     RESULTS,a2
        moveq   #28-1,d4
.sl     move.l  (a2)+,d3
        moveq   #8-1,d6
.sh     rol.l   #4,d3
        move.l  d3,d0
        and.w   #$f,d0
        add.w   #'0',d0
        cmp.w   #'9',d0
        ble     .sok
        addq.w  #7,d0           ; 'A'..'F'
.sok    bsr     sendb
        dbra    d6,.sh
        moveq   #13,d0          ; CR
        bsr     sendb
        moveq   #10,d0          ; LF
        bsr     sendb
        dbra    d4,.sl

        ; Bring up a single lores bitplane directly from the CPU (no copper):
        ; set the display registers once, then re-point the bitplane at the top
        ; of every frame so the image stays stable.
        move.w  #$1000,$100(a6) ; BPLCON0: 1 bitplane, lores
        move.w  #$0000,$102(a6) ; BPLCON1
        move.w  #$0000,$104(a6) ; BPLCON2
        move.w  #$0000,$108(a6) ; BPL1MOD
        move.w  #$0038,$092(a6) ; DDFSTRT
        move.w  #$00d0,$094(a6) ; DDFSTOP
        move.w  #$2c81,$08e(a6) ; DIWSTRT
        move.w  #$2cc1,$090(a6) ; DIWSTOP
        move.w  #$0000,$180(a6) ; COLOR00 black
        move.w  #$0fff,$182(a6) ; COLOR01 white
        move.w  #$8300,$096(a6) ; DMAEN | BPLEN
.show:
        bsr     syncframe
        move.l  #SCREEN,d0
        move.w  d0,$0e2(a6)     ; BPL1PTL
        swap    d0
        move.w  d0,$0e0(a6)     ; BPL1PTH
        bra     .show

;------------------------------------------------ VERTB interrupt handler
; Acks the VERTB request and returns. Touches no data/address register; the
; exception frame restores SR/CCR, so the interrupted measure loop is unperturbed
; except for the cycles the entry/handler/exit consumed.
vertb_handler:
        move.w  #$0020,$dff09c
        rte

;------------------------------------------------ interrupt-chain VERTB (level 3)
; Records the beam at entry (row 19 = interrupt latency from the vblank), acks
; VERTB, then SETs the level-1 SOFT request, mirroring a VERTB server that
; chains a cooperative task switch. SOFT (level 1) cannot preempt this level-3
; handler, so it is taken on the RTE below. Uses only
; d0/a0 and saves them so the interrupted measure loop is unperturbed.
vchain_handler:
        movem.l d0/a0,-(sp)
        lea     $dff000,a0
        move.w  $006(a0),CHAINEN+2 ; VHPOSR at entry -> low word of CHAINEN
        move.w  #$0020,$09c(a0) ; ack VERTB
        move.w  #$8004,$09c(a0) ; SET SOFT (request the level-1 switch)
        movem.l (sp)+,d0/a0
        rte

;------------------------------------------------ interrupt-chain SOFTINT (level 1)
; The cooperative task switch. Costs the same as a switcher: save all 15
; registers, two longword SP moves (mimic loading the other task's stack
; pointer), restore all registers. Records the beam at the END (row 20) so
; (row20 - row19) is the raster cost of the whole VERTB -> SET SOFT -> SOFT ->
; switch chain. Returns to the same context (no second task) so the measurement
; loop continues; only the cycles matter.
soft_handler:
        movem.l d0-d7/a0-a6,-(sp) ; save outgoing task's register file
        move.l  sp,SAVESP          ; "store SP into the outgoing task slot"
        move.l  SAVESP,sp          ; "load SP from the incoming task slot"
        movem.l (sp)+,d0-d7/a0-a6 ; restore incoming task's register file
        move.w  #$0004,$dff09c    ; ack SOFT
        move.w  $dff006,CHAINND+2 ; VHPOSR at chain end -> low word of row 20
        rte

;------------------------------------------------ copy the dbra template to (a1)
copytmpl:
        lea     slowtmpl(pc),a0
        move.w  #(slowtmpl_end-slowtmpl)/2-1,d2
.c      move.w  (a0)+,(a1)+
        dbra    d2,.c
        rts

;------------------------------------------------ relocatable measured dbra loop
        even
slowtmpl:
        move.w  #ITERS-1,d6
.l      dbra    d6,.l
        rts
slowtmpl_end:

;------------------------------------------------ copper list for the row-27 phase
; WAIT for line $64, raise the copper interrupt, then sleep. Read by the copper
; via DMA (it lives in chip RAM with the rest of the loaded program).
        even
copperlist:
        dc.w    $6401,$ff00     ; WAIT vp=$64, hp any (BFD set, VP fully compared)
        dc.w    $009c,$8010     ; MOVE #$8010,INTREQ -> SET COPER (raise IRQ)
        dc.w    $ffff,$fffe     ; WAIT forever (copper sleeps to end of frame)

;------------------------------------------------ send one char (d0.b) on serial
; Waits for SERDATR TBE (transmit buffer empty, bit 13) then writes SERDAT with
; the framing stop bit (bit 8). Independent of interrupts/DMA.
sendb:
.tbe    move.w  $018(a6),d1     ; SERDATR
        btst    #13,d1
        beq     .tbe
        and.w   #$ff,d0
        or.w    #$100,d0
        move.w  d0,$030(a6)     ; SERDAT
        rts

;------------------------------------------------ read the live beam vpos -> d0
; Combines VPOSR bit0 (V8) and VHPOSR high byte (V7..V0). DMA/interrupt
; independent and identical on every emulator.
getvpos:
        move.w  $004(a6),d0     ; VPOSR
        and.w   #1,d0           ; V8
        lsl.w   #8,d0           ; -> bit 8
        move.w  $006(a6),d1     ; VHPOSR
        lsr.w   #8,d1           ; V7..V0
        or.w    d1,d0           ; full vpos
        rts

;------------------------------------------------ linear beam position in cck
; Returns d0.l = vpos*227 + hpos, a monotonically increasing color-clock count
; within a frame (one PAL line = 227 cck; VHPOSR low byte is hpos/2). Used by the
; blitter-vs-beam rows to measure how far the beam advances while a fixed blit
; runs -- the direct "does the blitter keep ahead of the display beam" figure.
getbeam:
        move.w  $004(a6),d0     ; VPOSR
        and.w   #1,d0           ; V8
        lsl.w   #8,d0           ; -> bit 8
        move.w  $006(a6),d1     ; VHPOSR
        move.w  d1,d2
        lsr.w   #8,d1           ; V7..V0
        or.w    d1,d0           ; full vpos (d0.w)
        and.w   #$ff,d2         ; hpos/2
        add.w   d2,d2           ; hpos
        mulu    #227,d0         ; vpos * cck-per-line (d0.l)
        add.l   d2,d0           ; + hpos
        rts

;------------------------------------------------ wait for the blitter to finish
; Spin until DMACONR BBUSY (bit 14) clears. A short pre-spin lets BBUSY rise
; first (it is not asserted on the same cycle as the BLTSIZE write).
waitblit:
        moveq   #4,d0
.wpre   dbra    d0,.wpre        ; let BBUSY assert
.wbusy  move.w  $002(a6),d0     ; DMACONR
        btst    #14,d0          ; BBUSY still set?
        bne     .wbusy
        rts

;------------------------------------------------ sync to the next frame start
; Waits until the beam is near the bottom of a frame, then until it wraps back
; to the top. Detecting the wrap (rather than a single line) is robust even when
; the emulated beam advances in coarse steps between polls.
syncframe:
.hi     bsr     getvpos
        cmp.w   #280,d0
        blo     .hi             ; wait until near the bottom of the frame
.wrap   bsr     getvpos
        cmp.w   #280,d0
        bhs     .wrap           ; wait until vpos wraps back to the top
        rts

;------------------------------------------------ CIA-A timer A: start countdown
tstart:
        move.b  #$ff,$bfe401    ; TA low latch
        move.b  #$ff,$bfe501    ; TA high latch
        move.b  #$19,$bfee01    ; CRA: load + one-shot + start
        rts

;------------------------------------------------ read elapsed ticks -> d0.w
tread:
        move.b  #$08,$bfee01    ; CRA: stop (one-shot)
        moveq   #0,d0
        move.b  $bfe501,d0      ; TA high
        lsl.w   #8,d0
        move.b  $bfe401,d0      ; TA low
        not.w   d0             ; elapsed = $ffff - remaining
        rts

;------------------------------------------------ render 10 results as hex rows
render:
        lea     RESULTS,a2
        moveq   #0,d4           ; row index
.rr:
        move.l  (a2)+,d3        ; value
        move.w  d4,d0
        mulu    #360,d0         ; 9 scanlines * 40 bytes per row (27 rows fit)
        lea     SCREEN,a5
        adda.l  d0,a5           ; row top in bitplane
        moveq   #7,d6           ; 8 hex digits
        moveq   #0,d2           ; column (byte) index
.rd:
        rol.l   #4,d3           ; next nibble (high first) into low 4 bits
        move.l  d3,d0
        and.w   #$f,d0
        lsl.w   #3,d0           ; * 8 bytes per glyph
        lea     font(pc),a4
        adda.w  d0,a4
        move.l  a5,a1
        adda.w  d2,a1           ; + column byte
        moveq   #7,d5           ; 8 glyph rows
.rg:
        move.b  (a4)+,(a1)
        adda.w  #40,a1
        dbra    d5,.rg
        addq.w  #1,d2
        dbra    d6,.rd
        addq.w  #1,d4
        cmp.w   #28,d4
        bne     .rr
        rts

;------------------------------------------------ 8x8 hex font, glyphs 0..F
font:
        dc.b $70,$88,$98,$a8,$c8,$88,$70,$00    ; 0
        dc.b $20,$60,$20,$20,$20,$20,$70,$00    ; 1
        dc.b $70,$88,$08,$10,$20,$40,$f8,$00    ; 2
        dc.b $70,$88,$08,$30,$08,$88,$70,$00    ; 3
        dc.b $10,$30,$50,$90,$f8,$10,$10,$00    ; 4
        dc.b $f8,$80,$f0,$08,$08,$88,$70,$00    ; 5
        dc.b $30,$40,$80,$f0,$88,$88,$70,$00    ; 6
        dc.b $f8,$08,$10,$20,$40,$40,$40,$00    ; 7
        dc.b $70,$88,$88,$70,$88,$88,$70,$00    ; 8
        dc.b $70,$88,$88,$78,$08,$10,$60,$00    ; 9
        dc.b $70,$88,$88,$f8,$88,$88,$88,$00    ; A
        dc.b $f0,$88,$88,$f0,$88,$88,$f0,$00    ; B
        dc.b $70,$88,$80,$80,$80,$88,$70,$00    ; C
        dc.b $e0,$90,$88,$88,$88,$90,$e0,$00    ; D
        dc.b $f8,$80,$80,$f0,$80,$80,$f8,$00    ; E
        dc.b $f8,$80,$80,$f0,$80,$80,$80,$00    ; F

;------------------------------------------------ shared sprite data (8 sprites)
; VSTART=$50, VSTOP=$B4 (100 active lines); data words are zero (invisible) -- we
; only want the DMA fetches, not a visible sprite.
        even
spritedata:
        dc.w    $5040,$b400      ; SPRPOS / SPRCTL
        ds.w    200              ; 100 lines x 2 data words
        dc.w    $0000,$0000      ; end (VSTART=VSTOP=0)
