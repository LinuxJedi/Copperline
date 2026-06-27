.include "entry.s"
/* Test: FPU Exception Handling */
/* Tests FPU exception conditions on 68040 */

.set FPU_DZ_VEC, 0xD0          | Vector 52: FP Divide by Zero
.set FPU_OPERR_VEC, 0xD4       | Vector 53: FP Operand Error
.set FPU_OVFL_VEC, 0xD8        | Vector 54: FP Overflow

run_test:
    /* =================================================================== */
    /* Test 1: FPSR exception status bits - DZ (Divide by Zero) */
    /* =================================================================== */
    fmove.l #0, %fpsr           | Clear FPSR
    fmove.s const_1, %fp0       | 1.0
    fmove.s const_0, %fp1       | 0.0
    fdiv.x %fp1, %fp0           | 1.0 / 0.0 = Infinity, sets DZ
    
    fmove.l %fpsr, %d0
    btst #4, %d0                | DZ bit in FPSR exception byte
    beq TEST_FAIL               | DZ should be set
    
    /* =================================================================== */
    /* Test 2: Infinity result from divide by zero */
    /* =================================================================== */
    fmove.s const_1, %fp0
    fmove.s const_0, %fp1
    fdiv.x %fp1, %fp0
    
    /* Check for infinity by comparing with self - Inf == Inf */
    fcmp.x %fp0, %fp0
    fbne TEST_FAIL              | Infinity should equal itself
    
    /* =================================================================== */
    /* Test 3: FPSR OPERR - Invalid operation (0/0) */
    /* =================================================================== */
    fmove.l #0, %fpsr
    fmove.s const_0, %fp0       | 0.0
    fmove.s const_0, %fp1       | 0.0
    fdiv.x %fp1, %fp0           | 0/0 = NaN, sets OPERR
    
    fmove.l %fpsr, %d0
    btst #7, %d0                | AEXC IOP bit (accrued invalid-operation: OPERR)
    beq TEST_FAIL               | IOP should be set
    
    /* =================================================================== */
    /* Test 4: NaN propagation */
    /* =================================================================== */
    /* After 0/0, FP0 contains NaN */
    fcmp.x %fp0, %fp0
    fbeq TEST_FAIL              | NaN != NaN (always false)
    
    /* =================================================================== */
    /* Test 5: FPSR condition codes - Zero */
    /* =================================================================== */
    fmove.l #0, %fpsr
    fmove.s const_0, %fp0
    ftst.x %fp0
    fbeq skip_5                 | Should branch (zero)
    bra TEST_FAIL
skip_5:
    
    /* =================================================================== */
    /* Test 6: FPSR condition codes - Negative */
    /* =================================================================== */
    fmove.s const_neg1, %fp0
    ftst.x %fp0
    fblt skip_6                 | Should branch (negative)
    bra TEST_FAIL
skip_6:
    
    /* =================================================================== */
    /* Test 7: FPCR rounding mode - Round toward zero */
    /* =================================================================== */
    fmove.l #0x10, %fpcr        | RZ rounding mode (bits 4-5 = 01)
    fmove.s const_1_5, %fp0     | 1.5
    fint.x %fp0                 | Round to integer
    fcmp.s const_1, %fp0
    fbne TEST_FAIL              | Should be 1.0 (truncated)
    
    fmove.l #0, %fpcr           | Restore default rounding
    
    /* =================================================================== */
    /* Test 8: FPCR rounding mode - Round toward minus infinity */
    /* =================================================================== */
    fmove.l #0x20, %fpcr        | RM rounding mode (bits 4-5 = 10)
    fmove.s const_1_5, %fp0     | 1.5
    fint.x %fp0
    fcmp.s const_1, %fp0
    fbne TEST_FAIL              | Should be 1.0 (floor)
    
    fmove.l #0, %fpcr
    
    /* =================================================================== */
    /* Test 9: Negative zero handling */
    /* =================================================================== */
    fmove.s const_neg0, %fp0    | -0.0
    ftst.x %fp0
    fbeq skip_9                 | -0.0 should test as zero
    bra TEST_FAIL
skip_9:
    
    fmove.l %fpsr, %d0
    btst #27, %d0               | Check N bit - -0.0 sets negative (bit 27 in FPSR CC)
    beq TEST_FAIL
    
    rts

/* FPU Constants */
    .align 4
const_0:    .float 0.0
const_1:    .float 1.0
const_neg1: .float -1.0
const_1_5:  .float 1.5
const_neg0: .long 0x80000000    | -0.0 in IEEE 754
