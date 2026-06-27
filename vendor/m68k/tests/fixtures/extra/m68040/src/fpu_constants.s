.include "entry.s"
/* Test: FPU Special Values - Pi, e, ln2, etc. */
/* Tests FMOVECR (move constant ROM) instruction */

run_test:
    /* =================================================================== */
    /* Test 1: Load Pi from constant ROM */
    /* FMOVECR #$00 loads Pi = 3.14159265... */
    /* =================================================================== */
    fmovecr.x #0x00, %fp0       | fp0 = Pi
    
    /* Pi should be > 3 and < 4 */
    fmove.l #3, %fp7
    fcmp.x %fp7, %fp0
    fble TEST_FAIL              | Pi > 3
    
    fmove.l #4, %fp7
    fcmp.x %fp7, %fp0
    fbge TEST_FAIL              | Pi < 4
    
    /* =================================================================== */
    /* Test 2: Load log10(2) from constant ROM */
    /* FMOVECR #$0B loads log10(2) = 0.30103... */
    /* =================================================================== */
    fmovecr.x #0x0B, %fp1       | fp1 = log10(2)
    
    /* log10(2) should be > 0 and < 1 */
    ftst.x %fp1
    fble TEST_FAIL              | log10(2) > 0
    
    fmove.l #1, %fp7
    fcmp.x %fp7, %fp1
    fbge TEST_FAIL              | log10(2) < 1
    
    /* =================================================================== */
    /* Test 3: Load e from constant ROM */
    /* FMOVECR #$0C loads e = 2.718281828... */
    /* =================================================================== */
    fmovecr.x #0x0C, %fp2       | fp2 = e
    
    /* e should be > 2 and < 3 */
    fmove.l #2, %fp7
    fcmp.x %fp7, %fp2
    fble TEST_FAIL              | e > 2
    
    fmove.l #3, %fp7
    fcmp.x %fp7, %fp2
    fbge TEST_FAIL              | e < 3
    
    /* =================================================================== */
    /* Test 4: Load log2(e) from constant ROM */
    /* FMOVECR #$0D loads log2(e) = 1.442695... (ln(2) is at $30) */
    /* =================================================================== */
    fmovecr.x #0x0D, %fp3       | fp3 = log2(e)

    /* log2(e) should be > 1 and < 2 */
    fmove.l #1, %fp7
    fcmp.x %fp7, %fp3
    fble TEST_FAIL              | log2(e) > 1

    fmove.l #2, %fp7
    fcmp.x %fp7, %fp3
    fbge TEST_FAIL              | log2(e) < 2
    
    /* =================================================================== */
    /* Test 5: Load 0 from constant ROM */
    /* FMOVECR #$0F loads 0.0 */
    /* =================================================================== */
    fmovecr.x #0x0F, %fp4       | fp4 = 0
    
    ftst.x %fp4
    fbne TEST_FAIL              | Should be zero
    
    /* =================================================================== */
    /* Test 6: Load 1 from constant ROM */
    /* FMOVECR #$32 loads 1.0 */
    /* =================================================================== */
    fmovecr.x #0x32, %fp5       | fp5 = 1.0
    
    fmove.l #1, %fp7
    fcmp.x %fp7, %fp5
    fbne TEST_FAIL              | Should be 1.0
    
    rts
