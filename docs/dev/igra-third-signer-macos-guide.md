# IGRA Third Signer Guide (macOS)

Last updated: 2026-04-27, Asia/Jerusalem

This guide is for the non-technical signer.

Use it only when the IGRA team asks you to add the second signature to an exit transaction.

## Your Role

You do **not** build the transaction.
You do **not** edit the transaction.
You do **not** broadcast the transaction.

Your only job is:

1. receive the already prepared `signed-1.hex` file
2. sign it on the offline wallet machine
3. return the new `signed-2.hex` file

## Your Two Machines

- **Online Mac**: email, chat, file transfer, USB copy
- **Offline Mac**: holds the wallet keys and runs `kaspawallet`

Rules:

- The **offline Mac must stay offline**.
- Never connect the offline Mac to Wi-Fi.
- Never plug a network cable into the offline Mac.
- Never broadcast from the offline Mac.
- Never type or copy your wallet seed words anywhere except the official wallet when already expected.

## What You Will Receive

The IGRA operator will send you:

- one file ending with `.signed-1.hex`
- the expected Kaspa transaction ID

Example:

- file: `exit-6-official-bridge.signed-1.hex`
- txid: `97b1143e9b1ee398a875de4e0a88163b387a18885ee64f61a5cdb31d4bc5b0a8`

If you receive anything else, stop and ask.

## Before You Start

Prepare:

- one empty USB drive
- the offline Mac
- the `kaspawallet` app or binary on the offline Mac

Make sure the USB drive contains only the current signing file.

## Signing Steps

### 1. On the online Mac

1. Save the received `.signed-1.hex` file.
2. Copy that file to the USB drive.
3. Eject the USB drive.

### 2. On the offline Mac

1. Insert the USB drive.
2. Copy the `.signed-1.hex` file from the USB drive to the Desktop.
3. Open Terminal.
4. **Important: first go to the folder that contains `kaspawallet`.**

Example:

```bash
cd /path/to/kaspawallet-folder
```

5. **Only after that**, run this command:

```bash
./kaspawallet sign -F ~/Desktop/<file-name>.signed-1.hex > ~/Desktop/<file-name>.signed-2.hex
```

Example:

```bash
./kaspawallet sign -F ~/Desktop/exit-6-official-bridge.signed-1.hex > ~/Desktop/exit-6-official-bridge.signed-2.hex
```

6. Wait until the command finishes.
7. Confirm that a new file ending with `.signed-2.hex` now exists on the Desktop.
8. Copy the new `.signed-2.hex` file to the USB drive.
9. Eject the USB drive.

## After Signing

### 3. Back on the online Mac

1. Insert the USB drive.
2. Copy the `.signed-2.hex` file to the online Mac.
3. Send the `.signed-2.hex` file back to the IGRA operator.
4. Send this message:

```text
Signed successfully.
Returned file: <file-name>.signed-2.hex
Expected txid: <txid>
```

## What You Must Never Do

- Do **not** rename the file except the expected `.signed-1.hex` -> `.signed-2.hex`
- Do **not** open the file in a text editor and change anything
- Do **not** sign any file with a different name pattern
- Do **not** sign more than one file unless the operator clearly asked for that
- Do **not** broadcast the transaction
- Do **not** connect the offline Mac to the internet
- Do **not** share wallet keys, seed words, screenshots of seed words, or wallet files

## Stop And Ask For Help If

- the operator did not send an expected txid
- the file does not end with `.signed-1.hex`
- you are not sure which folder contains `kaspawallet`
- the command shows an error
- the wallet asks for something unexpected
- the `.signed-2.hex` file was not created
- there is more than one candidate file and you are not sure which one to sign

## Exact Message To Send When Something Is Wrong

```text
I stopped the signing process.
Please help me before I continue.
Issue: <short description>
```

## Best Format For This Guide

Use this guide as:

- a one-page PDF
- or a printed checklist kept near the offline Mac

Do not send her the full operator runbook. It is too long and too technical for this role.
