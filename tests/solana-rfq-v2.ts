import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import {
  createMint,
  createAssociatedTokenAccount,
  mintTo,
  getAccount,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { Keypair, SystemProgram, SYSVAR_INSTRUCTIONS_PUBKEY } from "@solana/web3.js";
import { expect } from "chai";
import { SolanaRfqV2 } from "../target/types/solana_rfq_v2";

describe("solana-rfq-v2 fill_exact_in", () => {
  anchor.setProvider(anchor.AnchorProvider.env());
  const provider = anchor.getProvider() as anchor.AnchorProvider;
  const program = anchor.workspace.solanaRfqV2 as Program<SolanaRfqV2>;

  let baseMint: anchor.web3.PublicKey;
  let quoteMint: anchor.web3.PublicKey;
  const baseDecimals = 9;
  const quoteDecimals = 6;
  let maker: Keypair;
  let user: Keypair;
  let userBase: anchor.web3.PublicKey;
  let userQuote: anchor.web3.PublicKey;
  let makerBase: anchor.web3.PublicKey;
  let makerQuote: anchor.web3.PublicKey;

  const airdrop = async (pk: anchor.web3.PublicKey, lamports: number) => {
    const sig = await provider.connection.requestAirdrop(pk, lamports);
    await provider.connection.confirmTransaction(sig);
  };

  before(async () => {
    maker = Keypair.generate();
    user = Keypair.generate();
    await airdrop(maker.publicKey, 5 * anchor.web3.LAMPORTS_PER_SOL);
    await airdrop(user.publicKey, 5 * anchor.web3.LAMPORTS_PER_SOL);

    const payer = (provider.wallet as anchor.Wallet).payer;
    baseMint = await createMint(provider.connection, payer, payer.publicKey, null, baseDecimals);
    quoteMint = await createMint(provider.connection, payer, payer.publicKey, null, quoteDecimals);

    userBase = await createAssociatedTokenAccount(provider.connection, payer, baseMint, user.publicKey);
    userQuote = await createAssociatedTokenAccount(provider.connection, payer, quoteMint, user.publicKey);
    makerBase = await createAssociatedTokenAccount(provider.connection, payer, baseMint, maker.publicKey);
    makerQuote = await createAssociatedTokenAccount(provider.connection, payer, quoteMint, maker.publicKey);

    await mintTo(provider.connection, payer, quoteMint, userQuote, payer, 100_000_000_000n);
    await mintTo(provider.connection, payer, baseMint, makerBase, payer, 1_000_000_000_000n);
  });

  const buildAccounts = () => ({
    user: user.publicKey,
    fillAuthority: maker.publicKey,
    userBaseTokenAccount: userBase,
    userQuoteTokenAccount: userQuote,
    makerBaseTokenAccount: makerBase,
    makerQuoteTokenAccount: makerQuote,
    baseMint,
    quoteMint,
    baseTokenProgram: TOKEN_PROGRAM_ID,
    quoteTokenProgram: TOKEN_PROGRAM_ID,
    instructionsSysvar: SYSVAR_INSTRUCTIONS_PUBKEY,
  });

  it("Bid: full consume across two levels, no dust", async () => {
    const levels = [
      { baseAtoms: new BN("100000000000"), quoteAtoms: new BN("8510000000") },
      { baseAtoms: new BN("200000000000"), quoteAtoms: new BN("17040000000") },
      { baseAtoms: new BN("300000000000"), quoteAtoms: new BN("25590000000") },
    ];
    const amountIn = new BN("25550000000");
    const expireAt = new BN(Math.floor(Date.now() / 1000) + 60);

    const userBaseBefore = await getAccount(provider.connection, userBase);

    await program.methods
      .fillExactIn({ bid: {} } as any, amountIn, {
        expireAt,
        minOutAtoms: new BN(0),
        levels,
      })
      .accounts(buildAccounts())
      .signers([user, maker])
      .rpc();

    const userBaseAfter = await getAccount(provider.connection, userBase);
    expect(Number(userBaseAfter.amount - userBaseBefore.amount)).to.equal(300_000_000_000);
  });

  it("Reverts on stale expire_at", async () => {
    const levels = [{ baseAtoms: new BN(100), quoteAtoms: new BN(85) }];
    const expireAt = new BN(Math.floor(Date.now() / 1000) - 10);
    try {
      await program.methods
        .fillExactIn({ bid: {} } as any, new BN(85), {
          expireAt,
          minOutAtoms: new BN(0),
          levels,
        })
        .accounts(buildAccounts())
        .signers([user, maker])
        .rpc();
      expect.fail("expected StaleOrderbook revert");
    } catch (e: any) {
      expect(e.toString()).to.match(/StaleOrderbook|6002/);
    }
  });

  it("Reverts on zero amount_in", async () => {
    const levels = [{ baseAtoms: new BN(100), quoteAtoms: new BN(85) }];
    const expireAt = new BN(Math.floor(Date.now() / 1000) + 60);
    try {
      await program.methods
        .fillExactIn({ bid: {} } as any, new BN(0), {
          expireAt,
          minOutAtoms: new BN(0),
          levels,
        })
        .accounts(buildAccounts())
        .signers([user, maker])
        .rpc();
      expect.fail("expected ZeroAmountIn revert");
    } catch (e: any) {
      expect(e.toString()).to.match(/ZeroAmountIn|6006/);
    }
  });

  it("Reverts on empty levels", async () => {
    const expireAt = new BN(Math.floor(Date.now() / 1000) + 60);
    try {
      await program.methods
        .fillExactIn({ bid: {} } as any, new BN(1), {
          expireAt,
          minOutAtoms: new BN(0),
          levels: [],
        })
        .accounts(buildAccounts())
        .signers([user, maker])
        .rpc();
      expect.fail("expected EmptyLevels revert");
    } catch (e: any) {
      expect(e.toString()).to.match(/EmptyLevels|6004/);
    }
  });

  it("Reverts on non-monotonic levels", async () => {
    const levels = [
      { baseAtoms: new BN(100), quoteAtoms: new BN(85) },
      { baseAtoms: new BN(100), quoteAtoms: new BN(85) },
    ];
    const expireAt = new BN(Math.floor(Date.now() / 1000) + 60);
    try {
      await program.methods
        .fillExactIn({ bid: {} } as any, new BN(170), {
          expireAt,
          minOutAtoms: new BN(0),
          levels,
        })
        .accounts(buildAccounts())
        .signers([user, maker])
        .rpc();
      expect.fail("expected InvalidLevelOrdering revert");
    } catch (e: any) {
      expect(e.toString()).to.match(/InvalidLevelOrdering|6003/);
    }
  });
});
