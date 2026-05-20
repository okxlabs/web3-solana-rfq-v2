#!/usr/bin/env ts-node
// solana-rfq-v2 CLI. Run `yarn rfq fill --help` for usage.

import * as fs from "fs";
import * as path from "path";

import * as anchor from "@coral-xyz/anchor";
import { BN, Program } from "@coral-xyz/anchor";
import {
  getAssociatedTokenAddressSync,
  getMint,
  TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import {
  Connection,
  Keypair,
  PublicKey,
  SYSVAR_INSTRUCTIONS_PUBKEY,
} from "@solana/web3.js";
import bs58 from "bs58";
import { Command } from "commander";
import { SolanaRfqV2 } from "../target/types/solana_rfq_v2";
const idl = require("../target/idl/solana_rfq_v2.json");

const WSOL_MINT = new PublicKey("So11111111111111111111111111111111111111112");
const USDC_MAINNET = new PublicKey(
  "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
);
const USDC_DEVNET = new PublicKey(
  "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU",
);
const DEFAULT_PROGRAM_ID = new PublicKey(
  "RFQ27dg5gSha2cDzQxuGyhfkz5CK2fUSy3Sjw4Rptyj",
);

type ClusterName = "mainnet" | "devnet" | "localnet";

function clusterDefaults(
  name: ClusterName | undefined,
): { rpc?: string; usdc?: PublicKey } {
  switch (name) {
    case "mainnet":
      return { rpc: "https://api.mainnet-beta.solana.com", usdc: USDC_MAINNET };
    case "devnet":
      return { rpc: "https://api.devnet.solana.com", usdc: USDC_DEVNET };
    case "localnet":
      return { rpc: "http://127.0.0.1:8899" };
    default:
      return {};
  }
}

// Key sources accepted:
//   /abs/or/relative/path.json   solana-keygen JSON array file
//   env:NAME                     read base58 or JSON array from env var NAME
//   [12,34,...]                  inline JSON array
//   <base58 string>              raw base58 secret key
function loadKeypair(src: string, fieldName: string): Keypair {
  const trimmed = src.trim();

  let raw: string;
  if (trimmed.startsWith("env:")) {
    const name = trimmed.slice(4);
    const v = process.env[name];
    if (!v) throw new Error(`${fieldName}: env var ${name} is unset`);
    raw = v.trim();
  } else if (trimmed.startsWith("[")) {
    raw = trimmed;
  } else if (fs.existsSync(trimmed)) {
    raw = fs.readFileSync(path.resolve(trimmed), "utf8").trim();
  } else {
    raw = trimmed;
  }

  try {
    if (raw.startsWith("[")) {
      return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(raw)));
    }
    return Keypair.fromSecretKey(bs58.decode(raw));
  } catch (e: any) {
    throw new Error(
      `${fieldName}: failed to parse keypair (${e.message ?? e})`,
    );
  }
}

const DECIMAL_RE = /^\d+(\.\d+)?$/;

// Truncates fractional digits beyond `decimals` (floor toward zero).
function decimalToAtoms(value: string, decimals: number, field: string): BN {
  const s = value.trim();
  if (!DECIMAL_RE.test(s)) {
    throw new Error(`${field}: must be a non-negative decimal, got "${value}"`);
  }
  const [whole, frac = ""] = s.split(".");
  const trimmedFrac = frac.slice(0, decimals);
  const padded = trimmedFrac.padEnd(decimals, "0");
  return new BN((whole + padded).replace(/^0+(?=\d)/, ""));
}

// Computes quote_atoms = floor(qty × price × 10^quoteDecimals) using exact
// integer math (no JS Number, no float).
function qtyPriceToAtoms(
  qty: string,
  price: string,
  baseDecimals: number,
  quoteDecimals: number,
): { baseAtoms: BN; quoteAtoms: BN } {
  const q = qty.trim();
  const p = price.trim();
  if (!DECIMAL_RE.test(q) || !DECIMAL_RE.test(p)) {
    throw new Error(
      `qty/price must be non-negative decimals; got qty="${qty}" price="${price}"`,
    );
  }
  const baseAtoms = decimalToAtoms(q, baseDecimals, "qty");

  const [qw, qf = ""] = q.split(".");
  const [pw, pf = ""] = p.split(".");
  const qScaled = new BN((qw + qf).replace(/^0+(?=\d)/, ""));
  const pScaled = new BN((pw + pf).replace(/^0+(?=\d)/, ""));
  const product = qScaled.mul(pScaled); // value = product × 10^-(qf.length + pf.length)
  const exp = quoteDecimals - qf.length - pf.length;

  const quoteAtoms = exp >= 0
    ? product.mul(new BN(10).pow(new BN(exp)))
    : product.div(new BN(10).pow(new BN(-exp))); // floor toward zero

  return { baseAtoms, quoteAtoms };
}

type LevelInput = { price: string | number; qty: string | number };

function parseLevels(
  src: string,
  baseDecimals: number,
  quoteDecimals: number,
): { price: string; qty: string; baseAtoms: BN; quoteAtoms: BN }[] {
  let raw = src.trim();
  if (raw.startsWith("@")) {
    raw = fs.readFileSync(path.resolve(raw.slice(1)), "utf8");
  }
  const parsed: LevelInput[] = JSON.parse(raw);
  if (!Array.isArray(parsed) || parsed.length === 0) {
    throw new Error("--levels must be a non-empty JSON array");
  }
  return parsed.map((l, i) => {
    if (l.price == null || l.qty == null) {
      throw new Error(`level[${i}] missing price/qty`);
    }
    const price = l.price.toString();
    const qty = l.qty.toString();
    const { baseAtoms, quoteAtoms } = qtyPriceToAtoms(
      qty,
      price,
      baseDecimals,
      quoteDecimals,
    );
    if (baseAtoms.isZero() || quoteAtoms.isZero()) {
      throw new Error(
        `level[${i}] produces zero atoms (qty=${qty}, price=${price}, base_dec=${baseDecimals}, quote_dec=${quoteDecimals})`,
      );
    }
    return { price, qty, baseAtoms, quoteAtoms };
  });
}

function parseSide(raw: string): { variant: any; tag: "bid" | "ask" } {
  const s = raw.toLowerCase();
  if (s === "bid") return { variant: { bid: {} }, tag: "bid" };
  if (s === "ask") return { variant: { ask: {} }, tag: "ask" };
  throw new Error(`--side must be "bid" or "ask", got "${raw}"`);
}

type FillOpts = {
  rpcUrl?: string;
  cluster?: ClusterName;
  makerKey: string;
  takerKey: string;
  side: string;
  amountIn: string;
  minOut: string;
  levels: string;
  baseMint?: string;
  quoteMint?: string;
  rfqId: string;
  expireAt?: string;
  programId: string;
  dryRun?: boolean;
};

async function runFill(opts: FillOpts) {
  const cluster = opts.cluster;
  const defaults = clusterDefaults(cluster);

  const rpcUrl = opts.rpcUrl ?? defaults.rpc;
  if (!rpcUrl) throw new Error("must pass --rpc-url or --cluster");

  const baseMint = new PublicKey(opts.baseMint ?? WSOL_MINT.toBase58());
  const quoteMintStr = opts.quoteMint ?? defaults.usdc?.toBase58();
  if (!quoteMintStr) {
    throw new Error("must pass --quote-mint or --cluster mainnet|devnet");
  }
  const quoteMint = new PublicKey(quoteMintStr);

  const maker = loadKeypair(opts.makerKey, "--maker-key");
  const taker = loadKeypair(opts.takerKey, "--taker-key");
  const side = parseSide(opts.side);
  const amountIn = new BN(opts.amountIn);
  const minOut = new BN(opts.minOut);
  const rfqId = new BN(opts.rfqId);
  const expireAt = new BN(opts.expireAt ?? Math.floor(Date.now() / 1000) + 60);
  const programId = new PublicKey(opts.programId);

  const connection = new Connection(rpcUrl, "confirmed");
  const [baseMintInfo, quoteMintInfo] = await Promise.all([
    getMint(connection, baseMint, "confirmed", TOKEN_PROGRAM_ID),
    getMint(connection, quoteMint, "confirmed", TOKEN_PROGRAM_ID),
  ]);
  const levels = parseLevels(
    opts.levels,
    baseMintInfo.decimals,
    quoteMintInfo.decimals,
  );
  const wallet = new anchor.Wallet(taker);
  const provider = new anchor.AnchorProvider(connection, wallet, {
    commitment: "confirmed",
    preflightCommitment: "confirmed",
  });
  anchor.setProvider(provider);

  const program = new Program(idl, provider) as unknown as Program<SolanaRfqV2>;
  if (!program.programId.equals(programId)) {
    throw new Error(
      `IDL programId ${program.programId} != --program-id ${programId}`,
    );
  }

  const userBase = getAssociatedTokenAddressSync(
    baseMint,
    taker.publicKey,
    false,
    TOKEN_PROGRAM_ID,
  );
  const userQuote = getAssociatedTokenAddressSync(
    quoteMint,
    taker.publicKey,
    false,
    TOKEN_PROGRAM_ID,
  );
  const makerBase = getAssociatedTokenAddressSync(
    baseMint,
    maker.publicKey,
    false,
    TOKEN_PROGRAM_ID,
  );
  const makerQuote = getAssociatedTokenAddressSync(
    quoteMint,
    maker.publicKey,
    false,
    TOKEN_PROGRAM_ID,
  );

  for (
    const [label, addr] of [
      ["user_base", userBase],
      ["user_quote", userQuote],
      ["maker_base", makerBase],
      ["maker_quote", makerQuote],
    ] as const
  ) {
    const info = await connection.getAccountInfo(addr);
    if (!info) {
      throw new Error(
        `ATA missing: ${label} ${addr.toBase58()} — create + fund it first`,
      );
    }
  }

  console.log("program:    ", programId.toBase58());
  console.log("rpc:        ", rpcUrl);
  console.log("taker(user):", taker.publicKey.toBase58());
  console.log("maker(auth):", maker.publicKey.toBase58());
  console.log("base_mint:  ", baseMint.toBase58());
  console.log("quote_mint: ", quoteMint.toBase58());
  console.log("side:       ", side.tag);
  console.log("amount_in:  ", amountIn.toString());
  console.log("min_out:    ", minOut.toString());
  console.log("rfq_id:     ", rfqId.toString());
  console.log("expire_at:  ", expireAt.toString());
  console.log(
    `decimals:    base=${baseMintInfo.decimals} quote=${quoteMintInfo.decimals}`,
  );
  console.log("levels:");
  for (const l of levels) {
    console.log(
      `  qty=${l.qty} @ price=${l.price}  →  base_atoms=${l.baseAtoms.toString()} quote_atoms=${l.quoteAtoms.toString()}`,
    );
  }

  const onchainLevels = levels.map((l) => ({
    baseAtoms: l.baseAtoms,
    quoteAtoms: l.quoteAtoms,
  }));
  const builder = program.methods
    .fillExactIn(side.variant, amountIn, minOut, {
      rfqId,
      expireAt,
      levels: onchainLevels,
    })
    .accounts({
      user: taker.publicKey,
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
    } as any)
    .signers([taker, maker]);

  if (opts.dryRun) {
    const tx = await builder.transaction();
    tx.feePayer = taker.publicKey;
    tx.recentBlockhash =
      (await connection.getLatestBlockhash("confirmed")).blockhash;
    tx.sign(taker, maker);
    const sim = await connection.simulateTransaction(tx);
    console.log("simulate err:", sim.value.err);
    console.log("logs:");
    for (const line of sim.value.logs ?? []) console.log(" ", line);
    return;
  }

  const sig = await builder.rpc({ commitment: "confirmed" });
  console.log("signature:  ", sig);
}

const cli = new Command();
cli
  .name("rfq")
  .description("solana-rfq-v2 CLI")
  .version("0.1.0");

cli
  .command("fill")
  .description("Send a fill_exact_in transaction (taker pays amount_in atoms)")
  .option(
    "--cluster <name>",
    "shortcut for rpc+usdc defaults: mainnet|devnet|localnet",
  )
  .option("--rpc-url <url>", "RPC endpoint (overrides --cluster)")
  .requiredOption(
    "--maker-key <src>",
    "maker (fill_authority) secret: file path, base58 string, or env:NAME",
  )
  .requiredOption(
    "--taker-key <src>",
    "taker (user) secret: file path, base58 string, or env:NAME",
  )
  .option("--side <bid|ask>", "taker side", "bid")
  .requiredOption("--amount-in <atoms>", "u64 input atoms")
  .option("--min-out <atoms>", "u64 slippage floor", "0")
  .requiredOption(
    "--levels <inline-json|@file>",
    'level array of {price, qty} in human units; e.g. \'[{"price":"200.5","qty":"10"}]\'. @path loads JSON from disk.',
  )
  .option("--base-mint <pubkey>", "base mint", WSOL_MINT.toBase58())
  .option("--quote-mint <pubkey>", "quote mint (defaults from --cluster)")
  .option("--rfq-id <u64>", "rfq id", "1")
  .option("--expire-at <unix>", "expire unix seconds (default: now+60)")
  .option("--program-id <pubkey>", "program id", DEFAULT_PROGRAM_ID.toBase58())
  .option("--dry-run", "simulate only, print logs, do not send")
  .action(async (opts: FillOpts) => {
    await runFill(opts);
  });

cli.parseAsync().catch((e) => {
  console.error(e.message ?? e);
  process.exit(1);
});

// yarn rfq fill \
//   --cluster mainnet \
//   --maker-key env:MAKER_PRIVATE_KEY \
//   --taker-key env:TAKER_PRIVATE_KEY \
//   --side bid \
//   --base-mint So11111111111111111111111111111111111111112 \
//   --quote-mint EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v \
//   --amount-in  100000 \
//   --levels '[{"price":"84.51","qty":"1"}]' \
//   --dry-run
