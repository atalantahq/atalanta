// SPDX-FileCopyrightText: the Atalanta authors
// SPDX-License-Identifier: AGPL-3.0-only

package okio;

/** Host-check stand-in for Okio's Timeout. */
public class Timeout {
    public static final Timeout NONE = new Timeout();
}
