#![no_std]
// `create_loan_request` legitimately needs 8 parameters (loan terms + reputation
// limits + collateral); clippy's macro backtrace attributes this lint to the
// `#[contractimpl]` expansion itself rather than a precise span, so a crate-level
// allow is required to suppress every attribution of the same underlying lint.
#![allow(clippy::too_many_arguments)]
use soroban_sdk::{
    contract, contractclient, contractimpl, contracttype, symbol_short, token, Address, Bytes,
    Env, Vec,
};

// ─── Flash loan callback interface ─────────────────────────────────────────────

/// Interface a receiving contract MUST implement to consume a flash loan.
///
/// `LendingContract::flash_loan` transfers `amount` of `token` to the receiver
/// *before* calling `execute_operation`, then — once the call returns — checks
/// that the pool's balance grew by at least `fee`. The receiver is therefore
/// responsible for transferring back `amount + fee` (or more) to the
/// LendingContract's address (`initiator`) from within this callback. If it
/// doesn't, `flash_loan` panics, which reverts the ENTIRE transaction —
/// including the initial transfer to the receiver — so funds can never be lost.
#[contractclient(name = "FlashLoanReceiverClient")]
pub trait FlashLoanReceiver {
    fn execute_operation(
        env: Env,
        token: Address,
        amount: i128,
        fee: i128,
        initiator: Address,
        params: Bytes,
    );
}

// ─── Types ────────────────────────────────────────────────────────────────────

/// Full lifecycle status of a loan.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoanStatus {
    Pending,
    Approved,
    Active,
    Repaid,
    Defaulted,
    Cancelled,
}

/// Interest rate model for a loan.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InterestRateModel {
    Fixed,
    Floating,
}

/// A single loan record.
#[contracttype]
#[derive(Clone)]
pub struct LoanRecord {
    pub id: u32,
    pub borrower: Address,
    pub lender: Address,
    /// Principal in stroops
    pub amount: i128,
    pub duration_days: u32,
    /// Interest rate in basis-points (1500 = 15.00 %)
    pub interest_rate_bps: u32,
    /// Principal + full interest in stroops
    pub total_due: i128,
    /// Remaining balance the borrower still owes
    pub remaining_due: i128,
    /// Ledger timestamp of loan creation
    pub created_at: u64,
    /// Ledger timestamp of repayment deadline
    pub due_at: u64,
    pub status: LoanStatus,
    /// Escrow ID from the EscrowContract
    pub escrow_id: u32,
    /// Platform fee taken (1% of interest, in stroops)
    pub platform_fee: i128,
    /// Collateral asset address (or XLM as default)
    pub collateral_asset: Address,
    /// Collateral amount in asset's smallest unit
    pub collateral_amount: i128,
    /// Interest rate model: Fixed or Floating
    pub rate_model: InterestRateModel,
    /// Baseline rate at loan creation in bps (anchors floating calculations)
    pub base_rate_bps: u32,
    /// Timestamp of the last floating rate adjustment
    pub last_rate_update: u64,
}

/// A partial/full payment record.
#[contracttype]
#[derive(Clone)]
pub struct PaymentRecord {
    pub loan_id: u32,
    pub amount: i128,
    pub paid_at: u64,
}

/// Ledger storage keys.
#[contracttype]
pub enum DataKey {
    Loan(u32),
    LoanCount,
    BorrowerLoans(Address),
    LenderLoans(Address),
    Payment(u32, u32), // (loan_id, payment_index)
    PaymentCount(u32), // per loan
    Admin,
    /// Platform fee as basis-points of interest (100 = 1.00%). DAO-controlled.
    PlatformFeeBps,
    /// Address of the Governance contract authorised to change the fee.
    Governance,
    /// Whitelisted collateral asset
    WhitelistedAsset(Address),
    /// Protocol flash-loan fee in basis-points of the borrowed amount.
    FlashLoanFeeBps,
    /// Address of the MultiSigAdmin contract — the ONLY caller authorised for
    /// rare, high-impact configuration changes (whitelisting assets, changing
    /// the flash-loan fee, linking governance). Once set, the plain `Admin`
    /// address can no longer call those functions directly.
    MultiSigAdmin,
    /// Tracks the last rate-model switch timestamp per loan (for cooldown).
    RateSwitchCooldown(u32),
}

/// Default platform fee = 1 % of interest (100 bps) until governance changes it.
const DEFAULT_PLATFORM_FEE_BPS: u32 = 100;
/// Safety ceiling: the fee can never exceed 10 % of interest (1000 bps),
/// even via a passed proposal.
const MAX_PLATFORM_FEE_BPS: u32 = 1000;

/// Default flash-loan fee = 0.09 % of the borrowed amount (9 bps) — in line
/// with common DeFi flash-loan pricing.
const DEFAULT_FLASH_LOAN_FEE_BPS: u32 = 9;
/// Safety ceiling on the flash-loan fee (500 bps = 5 %).
const MAX_FLASH_LOAN_FEE_BPS: u32 = 500;

/// Fee for switching rate models: 0.5% of remaining debt (50 bps).
const RATE_SWITCH_FEE_BPS: u32 = 50;

/// Cooldown between rate switches: 24 hours in seconds.
const RATE_SWITCH_COOLDOWN_SECS: u64 = 86_400;

// ─── Contract ─────────────────────────────────────────────────────────────────

#[contract]
pub struct LendingContract;

#[contractimpl]
impl LendingContract {
    // TODO (RWA Collateral Integration):
    // 1. Compatibility Check for Customized Asset Contracts:
    //    - Implement a validation helper `validate_rwa_token_compatibility(env: &Env, token_address: &Address)` to ensure
    //      the token contract implements the standard SEP-41 Token interface or custom compliance controls (clawback, transfer rules).
    //    - Store a whitelist of compatible tokenized assets (e.g. tokenized gold, US Treasury Bills) in instance storage.
    // 2. On-chain Oracle Price Feed Queries:
    //    - Integrate an oracle interface query to fetch real-time USD/XLM values for tokenized assets (e.g. XAU/USD, TBILL/USD).
    //    - Use the price feed to verify that the value of the deposited RWA collateral meets the required loan-to-value (LTV) ratio
    //      before approving or activating the loan.

    // ── Admin ─────────────────────────────────────────────────────────────────

    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic!("Contract already initialised");
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::LoanCount, &0u32);
        // Whitelist XLM as default collateral asset (using dummy address for now)
        // In real implementation, we'd use the native asset identifier
        env.storage().instance().set(&DataKey::WhitelistedAsset(admin.clone()), &true);
    }

    /// One-time bootstrap linking the MultiSigAdmin contract (admin only).
    /// Once set, this is the ONLY address that may call `whitelist_asset` /
    /// `set_flash_loan_fee_bps` / `set_governance` — the plain admin key loses
    /// direct access to these permanently. There is no unset/reset path other
    /// than the multisig's own internal signer governance.
    pub fn set_multisig_admin(env: Env, admin: Address, multisig: Address) {
        admin.require_auth();
        Self::assert_admin(&env, &admin);
        if env.storage().instance().has(&DataKey::MultiSigAdmin) {
            panic!("Multisig admin already configured");
        }
        env.storage().instance().set(&DataKey::MultiSigAdmin, &multisig);
    }

    pub fn get_multisig_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::MultiSigAdmin)
            .expect("Multisig admin not configured")
    }

    /// Whitelist a new collateral asset ("adding pools"). Multisig-gated —
    /// see `set_multisig_admin`.
    pub fn whitelist_asset(env: Env, caller: Address, asset: Address) {
        caller.require_auth();
        Self::assert_multisig_admin(&env, &caller);
        env.storage().instance().set(&DataKey::WhitelistedAsset(asset), &true);
    }

    /// Check if an asset is whitelisted
    pub fn is_asset_whitelisted(env: Env, asset: Address) -> bool {
        env.storage().instance().has(&DataKey::WhitelistedAsset(asset))
    }

    pub fn get_admin(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Contract not initialised")
    }

    // ── DAO governance of the platform fee ──────────────────────────────────────

    /// Link the Governance contract (multisig-gated, one-time bootstrap).
    /// Once set, the platform fee can ONLY be changed by this contract — i.e.
    /// by a successful on-chain vote.
    pub fn set_governance(env: Env, caller: Address, governance: Address) {
        caller.require_auth();
        Self::assert_multisig_admin(&env, &caller);
        env.storage().instance().set(&DataKey::Governance, &governance);
    }

    pub fn get_governance(env: Env) -> Address {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .expect("Governance not configured")
    }

    /// Current platform fee in basis-points of interest (default 100 = 1 %).
    pub fn get_platform_fee_bps(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::PlatformFeeBps)
            .unwrap_or(DEFAULT_PLATFORM_FEE_BPS)
    }

    /// Update the platform fee. Callable ONLY by the linked Governance contract,
    /// which invokes this after a proposal passes. This is the single on-chain
    /// path to changing the fee — there is intentionally no admin override.
    pub fn set_platform_fee_bps(env: Env, caller: Address, new_fee_bps: u32) {
        caller.require_auth();

        let governance: Address = env
            .storage()
            .instance()
            .get(&DataKey::Governance)
            .expect("Governance not configured");
        if caller != governance {
            panic!("Unauthorised: only Governance can change the platform fee");
        }
        if new_fee_bps > MAX_PLATFORM_FEE_BPS {
            panic!("Fee exceeds MAX_PLATFORM_FEE_BPS");
        }

        env.storage()
            .instance()
            .set(&DataKey::PlatformFeeBps, &new_fee_bps);
    }

    // ── Flash loans ──────────────────────────────────────────────────────────

    /// Current flash-loan fee in basis-points of the borrowed amount
    /// (default 9 = 0.09 %).
    pub fn get_flash_loan_fee_bps(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::FlashLoanFeeBps)
            .unwrap_or(DEFAULT_FLASH_LOAN_FEE_BPS)
    }

    /// Update the flash-loan fee ("interest rate table"), multisig-gated.
    /// Capped at `MAX_FLASH_LOAN_FEE_BPS`.
    pub fn set_flash_loan_fee_bps(env: Env, caller: Address, new_fee_bps: u32) {
        caller.require_auth();
        Self::assert_multisig_admin(&env, &caller);
        if new_fee_bps > MAX_FLASH_LOAN_FEE_BPS {
            panic!("Fee exceeds MAX_FLASH_LOAN_FEE_BPS");
        }
        env.storage()
            .instance()
            .set(&DataKey::FlashLoanFeeBps, &new_fee_bps);
    }

    /// Uncollateralized, single-transaction flash loan against the pool's own
    /// balance of `token`.
    ///
    /// Flow (all within this one call, hence one atomic ledger transaction):
    ///   1. Verify the pool holds at least `amount` of `token`.
    ///   2. Transfer `amount` of `token` to `receiver`.
    ///   3. Invoke `receiver.execute_operation(token, amount, fee, self, params)`
    ///      — the receiver's arbitrage/re-leveraging logic runs here and MUST
    ///      transfer `amount + fee` back to this contract before returning.
    ///   4. Verify the pool's balance grew by at least `fee`; if not, PANIC.
    ///
    /// A panic anywhere in this call — including inside the receiver's own
    /// callback — aborts the WHOLE transaction on Soroban, so step 2's transfer
    /// is rolled back along with everything else. There is no code path that
    /// leaves the pool short: either the loan is fully repaid plus fee, or the
    /// entire transaction (including the initial disbursement) never happened.
    pub fn flash_loan(env: Env, receiver: Address, token: Address, amount: i128, params: Bytes) {
        if amount <= 0 {
            panic!("Flash loan amount must be positive");
        }

        let token_client = token::Client::new(&env, &token);
        let pool = env.current_contract_address();
        let balance_before = token_client.balance(&pool);

        if balance_before < amount {
            panic!("Insufficient pool liquidity for flash loan");
        }

        let fee_bps = Self::get_flash_loan_fee_bps(env.clone());
        let fee = amount
            .checked_mul(fee_bps as i128)
            .expect("Overflow computing flash loan fee")
            / 10_000;
        let required_after = balance_before
            .checked_add(fee)
            .expect("Overflow computing required post-loan balance");

        // 2. Disburse the borrowed amount to the receiver.
        token_client.transfer(&pool, &receiver, &amount);

        // 3. Hand control to the receiver's callback.
        let receiver_client = FlashLoanReceiverClient::new(&env, &receiver);
        receiver_client.execute_operation(&token, &amount, &fee, &pool, &params);

        // 4. Enforce full repayment (principal + fee) — or roll back everything.
        let balance_after = token_client.balance(&pool);
        if balance_after < required_after {
            panic!("Flash loan not repaid: insufficient funds returned");
        }

        env.events().publish(
            (symbol_short!("flash"), symbol_short!("loan")),
            (receiver, token, amount, fee),
        );
    }

    // ── Loan lifecycle ────────────────────────────────────────────────────────

    /// Borrower creates a loan request.
    /// `interest_rate_bps` and `max_loan` are fetched off-chain from the
    /// ReputationContract and passed in so we avoid a cross-contract call
    /// on the critical path (cheaper, simpler on testnet).
    pub fn create_loan_request(
        env: Env,
        borrower: Address,
        amount: i128,
        duration_days: u32,
        interest_rate_bps: u32,
        max_loan_amount: i128,
        collateral_asset: Address,
        collateral_amount: i128,
        rate_model: InterestRateModel,
    ) -> u32 {
        borrower.require_auth();

        if amount <= 0 {
            panic!("Loan amount must be positive");
        }
        if amount > max_loan_amount {
            panic!("Amount exceeds reputation-based limit");
        }
        if duration_days == 0 || duration_days > 365 {
            panic!("Duration must be between 1 and 365 days");
        }
        if collateral_amount <= 0 {
            panic!("Collateral amount must be positive");
        }
        // Check if asset is whitelisted
        if !env.storage().instance().has(&DataKey::WhitelistedAsset(collateral_asset.clone())) {
            panic!("Collateral asset is not whitelisted");
        }

        // interest = principal × rate_bps × days / (10_000 × 365)
        let interest = Self::calculate_interest(amount, interest_rate_bps, duration_days);
        // Platform fee = (governance-controlled) fee_bps of interest.
        let fee_bps = Self::get_platform_fee_bps(env.clone());
        let platform_fee = interest
            .checked_mul(fee_bps as i128)
            .expect("Overflow: interest × fee_bps")
            / 10_000;
        let total_due = amount
            .checked_add(interest)
            .expect("Overflow computing total_due");

        let count: u32 = env
            .storage()
            .instance()
            .get(&DataKey::LoanCount)
            .unwrap_or(0);
        let loan_id = count + 1;

        let now = env.ledger().timestamp();
        // Compute due_at with overflow protection: days * 86_400 seconds
        let duration_secs: u64 = (duration_days as u64)
            .checked_mul(86_400)
            .expect("Overflow computing loan duration in seconds");
        let due_at = now
            .checked_add(duration_secs)
            .expect("Overflow computing due_at timestamp");

        let loan = LoanRecord {
            id: loan_id,
            borrower: borrower.clone(),
            lender: env.current_contract_address(), // placeholder until approved
            amount,
            duration_days,
            interest_rate_bps,
            total_due,
            remaining_due: total_due,
            created_at: now,
            due_at,
            status: LoanStatus::Pending,
            escrow_id: 0,
            platform_fee,
            collateral_asset,
            collateral_amount,
            rate_model,
            base_rate_bps: interest_rate_bps,
            last_rate_update: now,
        };

        env.storage().persistent().set(&DataKey::Loan(loan_id), &loan);
        env.storage().instance().set(&DataKey::LoanCount, &loan_id);

        // Track per-borrower list
        Self::push_loan_id_for_borrower(&env, &borrower, loan_id);

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("request")),
            (
                loan_id,
                borrower,
                amount,
                duration_days,
                interest_rate_bps,
                total_due,
                due_at,
            ),
        );

        loan_id
    }

    /// Lender approves a pending loan.
    pub fn approve_loan(
        env: Env,
        lender: Address,
        loan_id: u32,
        escrow_id: u32,
    ) {
        lender.require_auth();

        let mut loan = Self::get_loan(env.clone(), loan_id);
        if loan.status != LoanStatus::Pending {
            panic!("Loan is not in PENDING state");
        }

        loan.lender = lender.clone();
        loan.escrow_id = escrow_id;
        loan.status = LoanStatus::Approved;

        env.storage().persistent().set(&DataKey::Loan(loan_id), &loan);
        Self::push_loan_id_for_lender(&env, &lender, loan_id);

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("approved")),
            (loan_id, lender, escrow_id),
        );
    }

    /// Lender revokes an approved loan (within the 1-hour escrow window).
    /// The EscrowContract's `revoke_hold` must be called separately.
    pub fn revoke_approval(env: Env, lender: Address, loan_id: u32) {
        lender.require_auth();

        let mut loan = Self::get_loan(env.clone(), loan_id);
        if loan.lender != lender {
            panic!("Caller is not the lender");
        }
        if loan.status != LoanStatus::Approved {
            panic!("Loan is not in APPROVED state");
        }

        loan.status = LoanStatus::Pending;
        loan.lender = env.current_contract_address();
        loan.escrow_id = 0;
        env.storage().persistent().set(&DataKey::Loan(loan_id), &loan);

        env.events()
            .publish((symbol_short!("loan"), symbol_short!("revoked")), loan_id);
    }

    /// Admin/backend activates the loan once escrow disbursement is confirmed.
    pub fn activate_loan(env: Env, caller: Address, loan_id: u32) {
        caller.require_auth();
        Self::assert_admin(&env, &caller);

        let mut loan = Self::get_loan(env.clone(), loan_id);
        if loan.status != LoanStatus::Approved {
            panic!("Loan must be APPROVED before activation");
        }
        loan.status = LoanStatus::Active;
        env.storage().persistent().set(&DataKey::Loan(loan_id), &loan);

        env.events()
            .publish((symbol_short!("loan"), symbol_short!("active")), loan_id);
    }

    /// Record a repayment (partial or full).
    /// Actual XLM moves via PAYMENT op; admin calls this after Horizon confirm.
    pub fn record_payment(
        env: Env,
        caller: Address,
        loan_id: u32,
        amount: i128,
    ) -> LoanStatus {
        caller.require_auth();
        Self::assert_admin(&env, &caller);

        let mut loan = Self::get_loan(env.clone(), loan_id);
        if loan.status != LoanStatus::Active {
            panic!("Loan is not ACTIVE");
        }
        if amount <= 0 {
            panic!("Payment amount must be positive");
        }

        // Store payment record
        let payment_count: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::PaymentCount(loan_id))
            .unwrap_or(0);
        let new_count = payment_count + 1;
        let payment = PaymentRecord {
            loan_id,
            amount,
            paid_at: env.ledger().timestamp(),
        };
        env.storage()
            .persistent()
            .set(&DataKey::Payment(loan_id, new_count), &payment);
        env.storage()
            .persistent()
            .set(&DataKey::PaymentCount(loan_id), &new_count);

        // Reduce remaining balance (clamped to 0)
        if amount >= loan.remaining_due {
            loan.remaining_due = 0;
            loan.status = LoanStatus::Repaid;
        } else {
            loan.remaining_due -= amount;
        }

        env.storage().persistent().set(&DataKey::Loan(loan_id), &loan);
        env.events().publish(
            (symbol_short!("loan"), symbol_short!("payment")),
            (loan_id, amount, loan.remaining_due, loan.status.clone()),
        );
        loan.status
    }

    /// Mark a loan as defaulted (called by DefaultManagementContract or admin).
    pub fn mark_defaulted(env: Env, caller: Address, loan_id: u32) {
        caller.require_auth();
        Self::assert_admin(&env, &caller);

        let mut loan = Self::get_loan(env.clone(), loan_id);
        if loan.status != LoanStatus::Active {
            panic!("Only ACTIVE loans can be defaulted");
        }
        loan.status = LoanStatus::Defaulted;
        env.storage().persistent().set(&DataKey::Loan(loan_id), &loan);

        env.events()
            .publish((symbol_short!("loan"), symbol_short!("default")), loan_id);
    }

    // ── Rate model switching ─────────────────────────────────────────────────

    /// Borrower switches their loan between Fixed and Floating rate models.
    /// Charges a 0.5% fee on remaining debt and enforces a 24h cooldown.
    pub fn switch_rate_model(env: Env, borrower: Address, loan_id: u32) {
        borrower.require_auth();

        let mut loan = Self::get_loan(env.clone(), loan_id);
        if loan.borrower != borrower {
            panic!("Caller is not the borrower");
        }
        if loan.status != LoanStatus::Active {
            panic!("Can only switch rate model on ACTIVE loans");
        }

        // Enforce cooldown
        let now = env.ledger().timestamp();
        if let Some(last_switch) = env
            .storage()
            .persistent()
            .get::<DataKey, u64>(&DataKey::RateSwitchCooldown(loan_id))
        {
            if now.saturating_sub(last_switch) < RATE_SWITCH_COOLDOWN_SECS {
                panic!("Rate switch cooldown not elapsed (24h required)");
            }
        }

        // Charge switch fee: 0.5% of remaining debt
        let fee = loan
            .remaining_due
            .checked_mul(RATE_SWITCH_FEE_BPS as i128)
            .expect("Overflow computing switch fee")
            / 10_000;
        loan.remaining_due = loan
            .remaining_due
            .checked_add(fee)
            .expect("Overflow adding switch fee");
        loan.total_due = loan
            .total_due
            .checked_add(fee)
            .expect("Overflow adding switch fee to total");

        // Toggle model
        loan.rate_model = match loan.rate_model {
            InterestRateModel::Fixed => InterestRateModel::Floating,
            InterestRateModel::Floating => InterestRateModel::Fixed,
        };
        loan.last_rate_update = now;

        env.storage().persistent().set(&DataKey::Loan(loan_id), &loan);
        env.storage()
            .persistent()
            .set(&DataKey::RateSwitchCooldown(loan_id), &now);

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("rswitch")),
            (loan_id, loan.rate_model, fee),
        );
    }

    /// Admin updates the floating rate for a loan (called on state-changing interactions).
    /// Only applies to Floating-rate loans. Recalculates remaining interest.
    pub fn update_floating_rate(
        env: Env,
        caller: Address,
        loan_id: u32,
        new_rate_bps: u32,
    ) {
        caller.require_auth();
        Self::assert_admin(&env, &caller);

        let mut loan = Self::get_loan(env.clone(), loan_id);
        if loan.rate_model != InterestRateModel::Floating {
            panic!("Loan is not using floating rate model");
        }
        if loan.status != LoanStatus::Active {
            panic!("Can only update rate on ACTIVE loans");
        }

        let now = env.ledger().timestamp();

        // Compute remaining days
        let remaining_secs = if loan.due_at > now { loan.due_at - now } else { 0 };
        let remaining_days = (remaining_secs / 86_400) as u32;

        // Recalculate: amount already paid stays, recompute interest on remaining principal
        let paid_so_far = loan.total_due - loan.remaining_due;
        let remaining_principal = if loan.remaining_due > 0 {
            // Approximate remaining principal from remaining_due and old rate
            loan.amount
        } else {
            0
        };

        let new_interest =
            Self::calculate_interest(remaining_principal, new_rate_bps, remaining_days);
        let new_total_due = loan
            .amount
            .checked_add(new_interest)
            .expect("Overflow recomputing total_due");
        loan.total_due = new_total_due;
        loan.remaining_due = new_total_due
            .checked_sub(paid_so_far)
            .expect("Underflow computing new remaining_due");
        loan.interest_rate_bps = new_rate_bps;
        loan.last_rate_update = now;

        env.storage().persistent().set(&DataKey::Loan(loan_id), &loan);

        env.events().publish(
            (symbol_short!("loan"), symbol_short!("ratechg")),
            (loan_id, new_rate_bps, loan.remaining_due),
        );
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    pub fn get_loan(env: Env, loan_id: u32) -> LoanRecord {
        env.storage()
            .persistent()
            .get(&DataKey::Loan(loan_id))
            .expect("Loan not found")
    }

    pub fn get_loan_count(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::LoanCount)
            .unwrap_or(0)
    }

    /// Check whether a loan is overdue.
    pub fn is_overdue(env: Env, loan_id: u32) -> bool {
        let loan = Self::get_loan(env.clone(), loan_id);
        loan.status == LoanStatus::Active && env.ledger().timestamp() > loan.due_at
    }

    /// Days overdue (0 if not overdue yet).
    pub fn days_overdue(env: Env, loan_id: u32) -> u64 {
        let loan = Self::get_loan(env.clone(), loan_id);
        let now = env.ledger().timestamp();
        if loan.status == LoanStatus::Active && now > loan.due_at {
            (now - loan.due_at) / 86_400
        } else {
            0
        }
    }

    pub fn get_payment_count(env: Env, loan_id: u32) -> u32 {
        env.storage()
            .persistent()
            .get(&DataKey::PaymentCount(loan_id))
            .unwrap_or(0)
    }

    pub fn get_payment(env: Env, loan_id: u32, payment_index: u32) -> PaymentRecord {
        env.storage()
            .persistent()
            .get(&DataKey::Payment(loan_id, payment_index))
            .expect("Payment not found")
    }

    /// Calculate dynamic liquidation threshold based on borrower reputation score
    /// and asset volatility.
    ///
    /// - Base threshold: 7500 basis points (75.00%).
    /// - Reputation bonus: adds `reputation_score * 1.5` basis points (max 1500 bps).
    /// - Volatility penalty: subtracts `50%` of asset volatility bps.
    /// - Clamped between 5000 bps (50.00%) and 9000 bps (90.00%).
    /// - Uses checked arithmetic to prevent overflow.
    pub fn calculate_liquidation_threshold(
        _env: Env,
        borrower_reputation_score: u32,
        asset_volatility_bps: u32,
    ) -> u32 {
        let base_threshold: u32 = 7500;

        // reputation_bonus = borrower_reputation_score * 1.5
        let reputation_bonus = (borrower_reputation_score as u64)
            .checked_mul(15)
            .and_then(|v| v.checked_div(10))
            .expect("Overflow calculating reputation bonus");

        // volatility_penalty = asset_volatility_bps / 2
        let volatility_penalty = (asset_volatility_bps as u64)
            .checked_div(2)
            .expect("Overflow calculating volatility penalty");

        let threshold = (base_threshold as u64)
            .checked_add(reputation_bonus)
            .expect("Overflow adding reputation bonus")
            .saturating_sub(volatility_penalty);

        threshold.clamp(5000, 9000) as u32
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// interest = principal × rate_bps × days / (10_000 × 365)
    ///
    /// Uses checked arithmetic so that absurdly large principals or rates
    /// cause an explicit panic instead of silent integer wrap-around.
    fn calculate_interest(principal: i128, rate_bps: u32, days: u32) -> i128 {
        let numerator = principal
            .checked_mul(rate_bps as i128)
            .expect("Overflow: principal × rate_bps")
            .checked_mul(days as i128)
            .expect("Overflow: (principal × rate_bps) × days");
        numerator / (10_000_i128 * 365)
    }

    fn push_loan_id_for_borrower(env: &Env, borrower: &Address, loan_id: u32) {
        let key = DataKey::BorrowerLoans(borrower.clone());
        let mut ids: Vec<u32> = env.storage().persistent().get(&key).unwrap_or(Vec::new(env));
        ids.push_back(loan_id);
        env.storage().persistent().set(&key, &ids);
    }

    fn push_loan_id_for_lender(env: &Env, lender: &Address, loan_id: u32) {
        let key = DataKey::LenderLoans(lender.clone());
        let mut ids: Vec<u32> = env.storage().persistent().get(&key).unwrap_or(Vec::new(env));
        ids.push_back(loan_id);
        env.storage().persistent().set(&key, &ids);
    }

    fn assert_admin(env: &Env, caller: &Address) {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("Contract not initialised");
        if *caller != admin {
            panic!("Unauthorised: caller is not admin");
        }
    }

    fn assert_multisig_admin(env: &Env, caller: &Address) {
        let multisig: Address = env
            .storage()
            .instance()
            .get(&DataKey::MultiSigAdmin)
            .expect("Multisig admin not configured");
        if *caller != multisig {
            panic!("Unauthorised: caller is not the multisig admin");
        }
    }
}

#[cfg(test)]
mod test;
