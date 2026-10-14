<?php
/**
 * Link applier: pulls suggestions from the backend and splices links.
 *
 * @package siloforge-connect
 */

if ( ! defined( 'ABSPATH' ) ) {
	exit;
}

/**
 * Pulls SuggestionForPlugin rows, applies them to post content with
 * snapshots + anchor-nest guard, and acks each edge back to the API.
 */
class SFC_Linker {

	const SNAPSHOT_PREFIX   = '_sfc_snapshot_';
	const APPLIED_PREFIX    = '_sfc_applied_';
	const SNAPSHOTS_TO_KEEP = 3;

	/**
	 * API client.
	 *
	 * @var SFC_Client
	 */
	private $client;

	/**
	 * Constructor.
	 *
	 * @param SFC_Client $client API client.
	 */
	public function __construct( SFC_Client $client ) {
		$this->client = $client;
	}

	/**
	 * GET /api/v1/suggestions/{site_id}, apply each suggestion, ack each edge.
	 *
	 * @return array|WP_Error Report: total, applied, skipped, failed, posts_updated, details[].
	 */
	public function pull_and_apply() {
		$site_id = $this->client->get_site_id();
		if ( '' === $site_id ) {
			$handshake = $this->client->handshake();
			if ( is_wp_error( $handshake ) ) {
				return $handshake;
			}
			$site_id = $this->client->get_site_id();
			if ( '' === $site_id ) {
				return new WP_Error( 'sfc_no_site_id', __( 'No site id known; run Test Connection first.', 'siloforge-connect' ) );
			}
		}

		// Acks that could not be delivered last time (backend down after the post was saved) are
		// re-sent BEFORE fetching. Every edge that was queued is skipped in this run, delivered
		// or not: its link is already live, so it must never be applied a second time.
		$pending = $this->flush_pending_acks( $site_id );

		$suggestions = $this->client->authed_request( 'GET', '/api/v1/suggestions/' . rawurlencode( $site_id ) );
		if ( is_wp_error( $suggestions ) ) {
			return $suggestions;
		}
		if ( ! is_array( $suggestions ) ) {
			return new WP_Error( 'sfc_bad_response', __( 'Unexpected suggestions response from the SiloForge API.', 'siloforge-connect' ) );
		}

		$report = array(
			'total'         => count( $suggestions ),
			'applied'       => 0,
			'skipped'       => 0,
			'failed'        => 0,
			'posts_updated' => 0,
			'details'       => array(),
		);

		$grouped = array();
		foreach ( $suggestions as $suggestion ) {
			if ( is_array( $suggestion ) && isset( $suggestion['edge_id'] ) && isset( $pending[ (string) $suggestion['edge_id'] ] ) ) {
				$report['skipped']  += 1;
				$report['details'][] = array(
					'edge_id' => (string) $suggestion['edge_id'],
					'result'  => 'skipped',
					'reason'  => 'already applied by the plugin (acknowledgement re-sent)',
				);
				continue;
			}
			if ( ! is_array( $suggestion ) || ! isset( $suggestion['wp_post_id'], $suggestion['edge_id'] ) ) {
				$report['skipped'] += 1;
				$report['details'][] = array(
					'edge_id' => isset( $suggestion['edge_id'] ) ? (string) $suggestion['edge_id'] : '',
					'result'  => 'skipped',
					'reason'  => 'malformed suggestion',
				);
				continue;
			}
			$grouped[ (int) $suggestion['wp_post_id'] ][] = $suggestion;
		}

		foreach ( $grouped as $wp_post_id => $items ) {
			$this->apply_to_post( $site_id, $wp_post_id, $items, $report );
		}

		return $report;
	}

	/**
	 * Restore the newest content snapshot for a post.
	 *
	 * @param int $post_id Post ID.
	 * @return true|WP_Error
	 */
	public function revert_last( $post_id ) {
		$post_id = (int) $post_id;
		$keys    = $this->snapshot_keys( $post_id );
		if ( empty( $keys ) ) {
			return new WP_Error( 'sfc_no_snapshot', __( 'No SiloForge snapshot found for this post.', 'siloforge-connect' ) );
		}

		$newest  = end( $keys );
		$content = get_post_meta( $post_id, $newest, true );
		if ( ! is_string( $content ) ) {
			return new WP_Error( 'sfc_bad_snapshot', __( 'Snapshot content is missing or invalid.', 'siloforge-connect' ) );
		}

		$result = wp_update_post(
			wp_slash(
				array(
					'ID'           => $post_id,
					'post_content' => $content,
				)
			),
			true
		);
		if ( is_wp_error( $result ) ) {
			return $result;
		}

		// Tell the backend which edges were removed so its state matches the live
		// site and the edges become re-approvable.
		$suffix       = substr( $newest, strlen( self::SNAPSHOT_PREFIX ) );
		$applied_meta = get_post_meta( $post_id, self::APPLIED_PREFIX . $suffix, true );
		$edge_ids     = is_string( $applied_meta ) ? json_decode( $applied_meta, true ) : array();
		if ( is_array( $edge_ids ) ) {
			$site_id = $this->client->get_site_id();
			foreach ( $edge_ids as $edge_id ) {
				$this->revert_ack( $site_id, (string) $edge_id );
			}
		}
		delete_post_meta( $post_id, self::APPLIED_PREFIX . $suffix );

		delete_post_meta( $post_id, $newest );
		return true;
	}

	/**
	 * POST /api/v1/suggestions/{site_id}/revert-ack for one edge.
	 *
	 * @param string $site_id Site id.
	 * @param string $edge_id Edge id.
	 * @return void
	 */
	private function revert_ack( $site_id, $edge_id ) {
		if ( '' === $site_id || '' === $edge_id ) {
			return;
		}
		$this->client->authed_request(
			'POST',
			'/api/v1/suggestions/' . rawurlencode( $site_id ) . '/revert-ack',
			array( 'edge_id' => $edge_id )
		);
	}

	/**
	 * Apply all suggestions targeting one post.
	 *
	 * Every native suggestion that carries a `paragraph_hash` is verified first (see
	 * locate_paragraph()); a mismatch is skipped and acked `{ok:false, error:"content changed"}`.
	 * Successful edges are acked `ok:true` only AFTER wp_update_post() succeeded (R8); when
	 * the save fails they are acked as failures and the snapshot is discarded.
	 *
	 * @param string $site_id    Site id.
	 * @param int    $wp_post_id WordPress post ID.
	 * @param array  $items      Suggestions for this post.
	 * @param array  $report     Report accumulator (by reference).
	 * @return void
	 */
	private function apply_to_post( $site_id, $wp_post_id, $items, &$report ) {
		$post = get_post( $wp_post_id );
		if ( ! $post ) {
			foreach ( $items as $suggestion ) {
				$edge_id = (string) $suggestion['edge_id'];
				$this->ack( $site_id, $edge_id, false, 'post not found' );
				$report['failed']   += 1;
				$report['details'][] = array(
					'edge_id' => $edge_id,
					'result'  => 'failed',
					'reason'  => 'post not found',
				);
			}
			return;
		}

		$original = (string) $post->post_content;
		$content  = $original;
		$rendered = null;
		$applied  = array();

		foreach ( $items as $suggestion ) {
			$edge_id     = (string) $suggestion['edge_id'];
			$replacement = isset( $suggestion['replacement_html'] ) ? $suggestion['replacement_html'] : null;
			$verified    = '';

			if ( is_string( $replacement ) && '' !== $replacement ) {
				$index       = isset( $suggestion['paragraph_index'] ) ? $suggestion['paragraph_index'] : null;
				$new_content = $this->replace_paragraph( $content, $index, $replacement );
				$fail_reason = 'paragraph not found at index';
			} else {
				$anchor     = isset( $suggestion['anchor'] ) ? (string) $suggestion['anchor'] : '';
				$target_url = isset( $suggestion['target_url'] ) ? (string) $suggestion['target_url'] : '';
				$hash       = isset( $suggestion['paragraph_hash'] ) && is_string( $suggestion['paragraph_hash'] ) ? strtolower( trim( $suggestion['paragraph_hash'] ) ) : '';
				$index      = ( isset( $suggestion['paragraph_index'] ) && is_numeric( $suggestion['paragraph_index'] ) ) ? (int) $suggestion['paragraph_index'] : null;

				if ( '' === $anchor || '' === $target_url ) {
					$new_content = null;
					$fail_reason = 'missing anchor or target_url';
				} elseif ( '' !== $hash ) {
					if ( null === $rendered ) {
						$rendered = (string) apply_filters( 'the_content', $original );
					}
					$span = $this->locate_paragraph( $content, $rendered, $hash, $index, $anchor );
					if ( null === $span ) {
						$new_content = null;
						$fail_reason = 'content changed';
					} else {
						$verified    = $span['by'];
						$new_content = $this->insert_in_span( $content, $span, $anchor, $target_url );
						$fail_reason = 'anchor text not found outside existing links in the target paragraph';
					}
				} else {
					// No paragraph identity from the backend (old edge): first safe occurrence.
					$new_content = $this->insert_native( $content, $anchor, $target_url );
					$fail_reason = 'anchor text not found outside existing links';
				}
			}

			if ( null === $new_content ) {
				$this->ack( $site_id, $edge_id, false, $fail_reason );
				$report['skipped']  += 1;
				$report['details'][] = array(
					'edge_id' => $edge_id,
					'result'  => 'skipped',
					'reason'  => $fail_reason,
				);
				continue;
			}

			$content             = $new_content;
			$applied[ $edge_id ] = $verified;
		}

		if ( empty( $applied ) ) {
			return;
		}

		if ( $content === $original ) {
			// A replacement identical to the current text: nothing to save, the link is live.
			foreach ( $applied as $edge_id => $verified ) {
				$this->ack_or_queue( $site_id, (string) $edge_id, true, '' );
				$this->report_applied( $report, (string) $edge_id, $verified );
			}
			return;
		}

		$snapshot_suffix = $this->store_snapshot( $post->ID, $original );
		$result          = wp_update_post(
			wp_slash(
				array(
					'ID'           => $post->ID,
					'post_content' => $content,
				)
			),
			true
		);
		if ( is_wp_error( $result ) || 0 === $result ) {
			// Nothing was written: drop the snapshot and report every edge as failed.
			delete_post_meta( $post->ID, self::SNAPSHOT_PREFIX . $snapshot_suffix );
			$message = is_wp_error( $result ) ? $result->get_error_message() : 'unknown error';
			$error   = 'wp_update_post failed: ' . $message;
			foreach ( array_keys( $applied ) as $edge_id ) {
				$this->ack( $site_id, (string) $edge_id, false, $error );
				$report['failed']   += 1;
				$report['details'][] = array(
					'edge_id' => (string) $edge_id,
					'result'  => 'failed',
					'reason'  => $error,
				);
			}
			return;
		}

		$report['posts_updated'] += 1;
		update_post_meta( $post->ID, self::APPLIED_PREFIX . $snapshot_suffix, wp_json_encode( array_map( 'strval', array_keys( $applied ) ) ) );
		foreach ( $applied as $edge_id => $verified ) {
			$this->ack_or_queue( $site_id, (string) $edge_id, true, '' );
			$this->report_applied( $report, (string) $edge_id, $verified );
		}
	}

	/**
	 * Add an "applied" line to the report.
	 *
	 * @param array  $report   Report accumulator (by reference).
	 * @param string $edge_id  Edge id.
	 * @param string $verified How the paragraph was verified ('hash', 'anchor' or '').
	 * @return void
	 */
	private function report_applied( &$report, $edge_id, $verified ) {
		$report['applied'] += 1;
		$line               = array(
			'edge_id' => $edge_id,
			'result'  => 'applied',
		);
		if ( '' !== $verified ) {
			$line['verified'] = $verified;
		}
		$report['details'][] = $line;
	}

	/**
	 * POST /api/v1/suggestions/{site_id}/ack for one edge.
	 *
	 * @param string $site_id Site id.
	 * @param string $edge_id Edge id.
	 * @param bool   $ok      Whether the suggestion was applied.
	 * @param string $error   Optional error detail.
	 * @return bool True when the backend recorded the ack (or already had a final state).
	 */
	private function ack( $site_id, $edge_id, $ok, $error = '' ) {
		$body = array(
			'edge_id' => $edge_id,
			'ok'      => (bool) $ok,
		);
		if ( '' !== $error ) {
			$body['error'] = $error;
		}
		$result = $this->client->authed_request( 'POST', '/api/v1/suggestions/' . rawurlencode( $site_id ) . '/ack', $body );
		if ( ! is_wp_error( $result ) ) {
			return true;
		}
		// 404 (edge gone) and 409 (edge no longer approved) are final answers; retrying is pointless.
		$code = $result->get_error_code();
		return ( 'sfc_http_404' === $code || 'sfc_http_409' === $code );
	}

	/**
	 * Ack an edge whose link is already saved; if the backend cannot be reached, queue the
	 * ack (option sfc_pending_acks) so the next Pull & Apply delivers it and never re-applies.
	 *
	 * @param string $site_id Site id.
	 * @param string $edge_id Edge id.
	 * @param bool   $ok      Ack value.
	 * @param string $error   Optional error detail.
	 * @return void
	 */
	private function ack_or_queue( $site_id, $edge_id, $ok, $error ) {
		if ( $this->ack( $site_id, $edge_id, $ok, $error ) ) {
			return;
		}
		$pending = get_option( 'sfc_pending_acks', array() );
		if ( ! is_array( $pending ) ) {
			$pending = array();
		}
		$pending[ $edge_id ] = array(
			'site_id' => $site_id,
			'ok'      => (bool) $ok,
			'error'   => (string) $error,
			'time'    => time(),
		);
		update_option( 'sfc_pending_acks', $pending, false );
	}

	/**
	 * Retry queued acks; undelivered ones stay queued.
	 *
	 * @param string $site_id Current site id.
	 * @return array Every edge id that was queued at the start (edge_id => true).
	 */
	private function flush_pending_acks( $site_id ) {
		$pending = get_option( 'sfc_pending_acks', array() );
		if ( ! is_array( $pending ) || empty( $pending ) ) {
			return array();
		}
		$seen = array();
		$left = array();
		foreach ( $pending as $edge_id => $entry ) {
			$seen[ (string) $edge_id ] = true;
			if ( ! is_array( $entry ) ) {
				continue;
			}
			$entry_site = isset( $entry['site_id'] ) ? (string) $entry['site_id'] : '';
			if ( '' !== $entry_site && $entry_site !== $site_id ) {
				// Connected to another SiloForge site now; the old ack can never be delivered.
				continue;
			}
			$ok    = ! empty( $entry['ok'] );
			$error = isset( $entry['error'] ) ? (string) $entry['error'] : '';
			if ( ! $this->ack( $site_id, (string) $edge_id, $ok, $error ) ) {
				$left[ (string) $edge_id ] = $entry;
			}
		}
		update_option( 'sfc_pending_acks', $left, false );
		return $seen;
	}

	/**
	 * Find the paragraph a suggestion targets, verifying it against `paragraph_hash`.
	 *
	 * The backend hashes paragraphs with its canonical splitter (sf-engine html::paragraphs,
	 * Prose mode: SHA-1 of the whitespace-normalised, entity-decoded block text, blocks under
	 * 40 UTF-16 units dropped, duplicates dropped). That splitter cannot be reproduced exactly
	 * in PHP (list joining, shortcode/blocked-region rules), so verification is two-step:
	 *
	 * 1. hash: a block of the raw content (or of the rendered content, mapped back to the raw
	 *    block with the same text) whose approximate canonical text has exactly that SHA-1.
	 * 2. anchor: no hash match -> the block at `paragraph_index` (+/- 2, nearest first) must
	 *    contain the anchor text outside existing links.
	 *
	 * When neither holds, the post changed since the plan and the suggestion is skipped.
	 *
	 * @param string   $content  Current raw post_content.
	 * @param string   $rendered Rendered content (the_content) of the original post.
	 * @param string   $hash     Expected lowercase hex SHA-1.
	 * @param int|null $index    Backend paragraph index.
	 * @param string   $anchor   Anchor text.
	 * @return array|null Array( start, end, by ) byte span in $content, or null.
	 */
	private function locate_paragraph( $content, $rendered, $hash, $index, $anchor ) {
		$blocks = self::candidate_blocks( $content );

		foreach ( $blocks as $block ) {
			if ( in_array( $hash, $block['hashes'], true ) ) {
				return array(
					'start' => $block['start'],
					'end'   => $block['end'],
					'by'    => 'hash',
				);
			}
		}

		// The backend may have hashed the rendered content (plugin sync sends the_content):
		// find the rendered block, then the raw block with the same text.
		if ( '' !== $rendered && $rendered !== $content ) {
			$wanted = null;
			foreach ( self::candidate_blocks( $rendered ) as $block ) {
				if ( in_array( $hash, $block['hashes'], true ) ) {
					$wanted = $block['hashes'];
					break;
				}
			}
			if ( null !== $wanted ) {
				foreach ( $blocks as $block ) {
					if ( array() !== array_intersect( $wanted, $block['hashes'] ) ) {
						return array(
							'start' => $block['start'],
							'end'   => $block['end'],
							'by'    => 'hash',
						);
					}
				}
			}
		}

		if ( null === $index || $index < 0 ) {
			return null;
		}
		$ordered = self::ordered_blocks( $blocks );
		foreach ( array( 0, -1, 1, -2, 2 ) as $delta ) {
			$i = $index + $delta;
			if ( ! isset( $ordered[ $i ] ) ) {
				continue;
			}
			if ( $this->span_has_anchor( $content, $ordered[ $i ], $anchor ) ) {
				return array(
					'start' => $ordered[ $i ]['start'],
					'end'   => $ordered[ $i ]['end'],
					'by'    => 'anchor',
				);
			}
		}
		return null;
	}

	/**
	 * Whether the anchor occurs (outside links) inside a block of $content.
	 *
	 * @param string $content Raw content.
	 * @param array  $block   Block with start/end.
	 * @param string $anchor  Anchor text.
	 * @return bool
	 */
	private function span_has_anchor( $content, $block, $anchor ) {
		$fragment = substr( $content, $block['start'], $block['end'] - $block['start'] );
		return null !== $this->insert_native( $fragment, $anchor, 'https://example.invalid/' );
	}

	/**
	 * Insert the link inside one byte span of $content and splice it back.
	 *
	 * @param string $content    Raw content.
	 * @param array  $span       Array with start/end.
	 * @param string $anchor     Anchor text.
	 * @param string $target_url Link target.
	 * @return string|null
	 */
	private function insert_in_span( $content, $span, $anchor, $target_url ) {
		$length   = $span['end'] - $span['start'];
		$fragment = substr( $content, $span['start'], $length );
		$linked   = $this->insert_native( $fragment, $anchor, $target_url );
		if ( null === $linked ) {
			return null;
		}
		return substr_replace( $content, $linked, $span['start'], $length );
	}

	/**
	 * Candidate paragraph blocks of an HTML/raw document, each with its byte span, the
	 * approximate canonical text and the SHA-1 of every text variant. Blocks may overlap.
	 *
	 * @param string $html Document.
	 * @return array[] Each: start, end, text, hashes[], tag ('' for a blank-line run), blocky.
	 */
	public static function candidate_blocks( $html ) {
		$html   = (string) $html;
		$blocks = array();
		$tags   = array( 'p', 'li', 'ul', 'ol', 'blockquote', 'td', 'th', 'dd', 'dt', 'figcaption', 'table', 'div', 'section' );
		foreach ( $tags as $tag ) {
			foreach ( self::element_spans( $html, $tag ) as $span ) {
				$blocks[] = self::make_block( substr( $html, $span[0], $span[1] - $span[0] ), $span[0], $tag );
			}
		}

		// Blank-line separated runs (classic/raw content that wpautop turns into paragraphs).
		$parts = preg_split( '/\n[ \t\r]*\n/', $html, -1, PREG_SPLIT_OFFSET_CAPTURE | PREG_SPLIT_NO_EMPTY );
		if ( is_array( $parts ) ) {
			foreach ( $parts as $part ) {
				$raw   = $part[0];
				$trim  = ltrim( $raw );
				$start = (int) $part[1] + ( strlen( $raw ) - strlen( $trim ) );
				$trim  = rtrim( $trim );
				if ( '' === $trim ) {
					continue;
				}
				$blocks[] = self::make_block( $trim, $start, '' );
			}
		}

		$out = array();
		foreach ( $blocks as $block ) {
			if ( self::utf16_length( $block['text'] ) >= 40 ) {
				$out[] = $block;
			}
		}
		return $out;
	}

	/**
	 * Byte spans [start, end) of every <$tag> element, matching nested same-name tags.
	 * Bounded: at most 2000 elements per tag; an unclosed element is ignored.
	 *
	 * @param string $html Document.
	 * @param string $tag  Lowercase tag name.
	 * @return array[] Each: array( start, end ).
	 */
	private static function element_spans( $html, $tag ) {
		if ( false === stripos( $html, '<' . $tag ) ) {
			return array();
		}
		if ( ! preg_match_all( '#<(/?)' . $tag . '\b[^>]*>#i', $html, $matches, PREG_OFFSET_CAPTURE | PREG_SET_ORDER ) ) {
			return array();
		}
		$spans = array();
		$stack = array();
		foreach ( $matches as $match ) {
			$offset = (int) $match[0][1];
			if ( '' === $match[1][0] ) {
				if ( '/' === substr( $match[0][0], -2, 1 ) ) {
					continue; // Self-closing; holds no text.
				}
				$stack[] = $offset;
				continue;
			}
			if ( empty( $stack ) ) {
				continue;
			}
			$start   = array_pop( $stack );
			$spans[] = array( $start, $offset + strlen( $match[0][0] ) );
			if ( count( $spans ) >= 2000 ) {
				break;
			}
		}
		usort(
			$spans,
			static function ( $a, $b ) {
				return $a[0] <=> $b[0];
			}
		);
		return $spans;
	}

	/**
	 * Build one block record.
	 *
	 * The backend's text for a block depends on how its splitter joins elements, and older
	 * (v1-era) paragraph rows used other joins, so three variants are hashed: block tags
	 * separated (the usual case), everything joined tightly (lists, tables of contents) and
	 * every tag separated (v1 rows, `<strong>Label</strong>: text` read as "Label : text").
	 *
	 * @param string $source Exact block source.
	 * @param int    $start  Byte offset.
	 * @param string $tag    Tag name or ''.
	 * @return array
	 */
	private static function make_block( $source, $start, $tag ) {
		$text   = self::canonical_text( $source, 'block' );
		$hashes = array( sha1( $text ) );
		foreach ( array( 'none', 'all' ) as $mode ) {
			$variant = sha1( self::canonical_text( $source, $mode ) );
			if ( ! in_array( $variant, $hashes, true ) ) {
				$hashes[] = $variant;
			}
		}
		$block_re = '#<(?:p|ul|ol|li|blockquote|table|h[1-6]|div|figure|section)\b#i';
		$gt       = strpos( $source, '>' );
		$inner    = ( false === $gt ) ? '' : substr( $source, $gt + 1 );
		return array(
			'start'        => $start,
			'end'          => $start + strlen( $source ),
			'text'         => $text,
			'hash'         => $hashes[0],
			'hashes'       => $hashes,
			'tag'          => $tag,
			'blocky'       => ( 1 === preg_match( $block_re, $source ) ),
			'inner_blocky' => ( 1 === preg_match( $block_re, $inner ) ),
		);
	}

	/**
	 * Document-order paragraph list approximating the backend's `paragraph_index`:
	 * <p>, whole lists, blockquote/table cells without inner <p>, and blank-line runs
	 * holding no block markup; nested blocks dropped, duplicates (same text) dropped.
	 *
	 * @param array[] $blocks Output of candidate_blocks().
	 * @return array[]
	 */
	private static function ordered_blocks( $blocks ) {
		$keep = array();
		foreach ( $blocks as $block ) {
			$tag = $block['tag'];
			if ( 'li' === $tag || 'table' === $tag ) {
				continue;
			}
			if ( ( 'div' === $tag || 'section' === $tag ) && $block['inner_blocky'] ) {
				continue;
			}
			if ( '' === $tag && $block['blocky'] ) {
				continue;
			}
			$keep[] = $block;
		}
		usort(
			$keep,
			static function ( $a, $b ) {
				if ( $a['start'] !== $b['start'] ) {
					return $a['start'] <=> $b['start'];
				}
				return $b['end'] <=> $a['end'];
			}
		);
		$out  = array();
		$seen = array();
		foreach ( $keep as $block ) {
			$inside = false;
			foreach ( $out as $prev ) {
				if ( $block['start'] >= $prev['start'] && $block['end'] <= $prev['end'] ) {
					$inside = true;
					break;
				}
			}
			$covers_p = false;
			foreach ( $keep as $other ) {
				if ( 'p' === $other['tag'] && $other !== $block && $other['start'] >= $block['start'] && $other['end'] <= $block['end'] ) {
					$covers_p = true;
					break;
				}
			}
			if ( $inside || ( 'p' !== $block['tag'] && $covers_p ) || isset( $seen[ $block['hash'] ] ) ) {
				continue;
			}
			$seen[ $block['hash'] ] = true;
			$out[]                  = $block;
		}
		return $out;
	}

	/**
	 * Approximate the backend's canonical paragraph text: drop comments, script/style,
	 * shortcode tags and markup; decode entities; collapse JavaScript whitespace; trim.
	 *
	 * @param string $html Block source.
	 * @param string $sep  Where tags become a space: 'block' (block-level tags and <br>),
	 *                     'none' (nowhere, tight join) or 'all' (every tag).
	 * @return string
	 */
	public static function canonical_text( $html, $sep = 'block' ) {
		$text = (string) $html;
		$text = (string) preg_replace( '#<!--.*?-->#s', '', $text );
		$text = (string) preg_replace( '#<(script|style|noscript|template)\b[^>]*>.*?</\1\s*>#is', '', $text );
		$text = (string) preg_replace( '#\[/?[A-Za-z][\w-]*(?:\s[^\]]*)?/?\]#', '', $text );
		if ( 'all' === $sep ) {
			$text = (string) preg_replace( '#<[^>]*>#', ' ', $text );
		} else {
			$text = (string) preg_replace( '#<(?:br|/?(?:p|li|ul|ol|div|section|table|caption|thead|tbody|blockquote|td|th|tr|dd|dt|h[1-6]|figcaption))\b[^>]*>#i', 'none' === $sep ? '' : ' ', $text );
		}
		$text = (string) preg_replace( '#<[^>]*>#', '', $text );
		$text = html_entity_decode( $text, ENT_QUOTES | ENT_HTML5, 'UTF-8' );
		$ws   = '[\t\n\x{0B}\x{0C}\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]+';
		$norm = preg_replace( '/' . $ws . '/u', ' ', $text );
		if ( null === $norm ) {
			// Invalid UTF-8: fall back to ASCII whitespace only.
			$norm = (string) preg_replace( '/[\t\n\x0B\x0C\r ]+/', ' ', $text );
		}
		return trim( $norm, ' ' );
	}

	/**
	 * Length in UTF-16 code units (JavaScript `.length`), as the backend's MIN_PARAGRAPH_CHARS uses.
	 *
	 * @param string $text UTF-8 text.
	 * @return int
	 */
	private static function utf16_length( $text ) {
		if ( function_exists( 'mb_convert_encoding' ) ) {
			$utf16 = mb_convert_encoding( (string) $text, 'UTF-16LE', 'UTF-8' );
			if ( is_string( $utf16 ) ) {
				return (int) ( strlen( $utf16 ) / 2 );
			}
		}
		return strlen( (string) $text );
	}

	/**
	 * Replace the Nth top-level paragraph with replacement HTML.
	 *
	 * When the stored content already contains <p> tags they are used
	 * directly (they are what wpautop would keep); otherwise the content
	 * is split into the double-newline blocks wpautop would wrap.
	 *
	 * @param string   $content          Raw post_content.
	 * @param int|null $index            Zero-based paragraph index.
	 * @param string   $replacement_html Replacement paragraph HTML.
	 * @return string|null New content, or null when the paragraph was not found.
	 */
	private function replace_paragraph( $content, $index, $replacement_html ) {
		if ( null === $index || ! is_numeric( $index ) ) {
			return null;
		}
		$index = (int) $index;
		if ( $index < 0 ) {
			return null;
		}

		if ( false !== stripos( $content, '<p' ) ) {
			if ( ! preg_match_all( '#<p\b[^>]*>.*?</p>#is', $content, $matches, PREG_OFFSET_CAPTURE ) ) {
				return null;
			}
			if ( ! isset( $matches[0][ $index ] ) ) {
				return null;
			}
			$match = $matches[0][ $index ];
			return substr_replace( $content, $replacement_html, $match[1], strlen( $match[0] ) );
		}

		$blocks = preg_split( '/\n\s*\n/', $content );
		if ( ! is_array( $blocks ) || ! isset( $blocks[ $index ] ) ) {
			return null;
		}
		$blocks[ $index ] = $replacement_html;
		return implode( "\n\n", $blocks );
	}

	/**
	 * Wrap the first occurrence of the anchor text that is not already
	 * inside an <a> tag with a link to the target URL.
	 *
	 * @param string $content    Raw post_content.
	 * @param string $anchor     Anchor text (matched case-insensitively).
	 * @param string $target_url Link target.
	 * @return string|null New content, or null when no safe occurrence exists.
	 */
	private function insert_native( $content, $anchor, $target_url ) {
		if ( class_exists( 'DOMDocument' ) && class_exists( 'DOMXPath' ) ) {
			$dom_result = $this->insert_native_dom( $content, $anchor, $target_url );
			if ( false !== $dom_result ) {
				// Definitive answer from the DOM pass: string (new content) or null (not found).
				return $dom_result;
			}
		}
		return $this->insert_native_regex( $content, $anchor, $target_url );
	}

	/**
	 * DOM-based native insertion over text nodes with no <a> ancestor.
	 *
	 * @param string $content    Raw post_content.
	 * @param string $anchor     Anchor text.
	 * @param string $target_url Link target.
	 * @return string|null|false New content, null when not found, false when parsing failed.
	 */
	private function insert_native_dom( $content, $anchor, $target_url ) {
		$doc  = new DOMDocument();
		$prev = libxml_use_internal_errors( true );

		$flags = 0;
		if ( defined( 'LIBXML_HTML_NOIMPLIED' ) ) {
			$flags |= LIBXML_HTML_NOIMPLIED;
		}
		if ( defined( 'LIBXML_HTML_NODEFDTD' ) ) {
			$flags |= LIBXML_HTML_NODEFDTD;
		}

		$loaded = $doc->loadHTML( '<?xml encoding="utf-8" ?><div id="sfc-root">' . $content . '</div>', $flags );
		libxml_clear_errors();
		libxml_use_internal_errors( $prev );
		if ( ! $loaded ) {
			return false;
		}

		$xpath = new DOMXPath( $doc );
		$roots = $xpath->query( '//div[@id="sfc-root"]' );
		if ( ! $roots || 0 === $roots->length ) {
			return false;
		}
		$root = $roots->item( 0 );

		$nodes = $xpath->query( './/text()[not(ancestor::a)]', $root );
		if ( false === $nodes ) {
			return false;
		}

		foreach ( $nodes as $node ) {
			$parts = $this->split_at_match( (string) $node->nodeValue, $anchor );
			if ( null === $parts ) {
				continue;
			}

			$link = $doc->createElement( 'a' );
			$link->setAttribute( 'href', esc_url_raw( $target_url ) );
			$link->appendChild( $doc->createTextNode( $parts['match'] ) );

			$parent = $node->parentNode;
			if ( ! $parent ) {
				continue;
			}
			$parent->insertBefore( $doc->createTextNode( $parts['before'] ), $node );
			$parent->insertBefore( $link, $node );
			$node->nodeValue = $parts['after'];

			$output = '';
			foreach ( $root->childNodes as $child ) {
				$output .= $doc->saveHTML( $child );
			}
			return $output;
		}

		return null;
	}

	/**
	 * Regex fallback: first case-insensitive anchor occurrence that is
	 * neither inside a tag nor inside an existing <a>...</a> run.
	 *
	 * @param string $content    Raw post_content.
	 * @param string $anchor     Anchor text.
	 * @param string $target_url Link target.
	 * @return string|null
	 */
	private function insert_native_regex( $content, $anchor, $target_url ) {
		$pattern = '/' . preg_quote( $anchor, '/' ) . '(?![^<]*>)(?![^<]*<\/a>)/i';
		$count   = 0;
		$new     = preg_replace_callback(
			$pattern,
			static function ( $matches ) use ( $target_url ) {
				return '<a href="' . esc_url( $target_url ) . '">' . $matches[0] . '</a>';
			},
			$content,
			1,
			$count
		);
		if ( null === $new || 0 === $count ) {
			return null;
		}
		return $new;
	}

	/**
	 * Case-insensitive split of $text around the first $anchor occurrence.
	 *
	 * @param string $text   Haystack text.
	 * @param string $anchor Needle.
	 * @return array|null Array with before/match/after, or null when absent.
	 */
	private function split_at_match( $text, $anchor ) {
		if ( function_exists( 'mb_stripos' ) && function_exists( 'mb_substr' ) && function_exists( 'mb_strlen' ) ) {
			$pos = mb_stripos( $text, $anchor );
			if ( false === $pos ) {
				return null;
			}
			$len = mb_strlen( $anchor );
			return array(
				'before' => mb_substr( $text, 0, $pos ),
				'match'  => mb_substr( $text, $pos, $len ),
				'after'  => mb_substr( $text, $pos + $len ),
			);
		}

		$pos = stripos( $text, $anchor );
		if ( false === $pos ) {
			return null;
		}
		$len = strlen( $anchor );
		return array(
			'before' => substr( $text, 0, $pos ),
			'match'  => substr( $text, $pos, $len ),
			'after'  => substr( $text, $pos + $len ),
		);
	}

	/**
	 * Store an original-content snapshot as post meta, keeping the last 3.
	 *
	 * @param int    $post_id Post ID.
	 * @param string $content Original post_content.
	 * @return string The suffix used for the snapshot (and its paired applied) meta key.
	 */
	private function store_snapshot( $post_id, $content ) {
		// time() alone (1-second resolution) collides when a post is applied to
		// twice within the same second. Append a short unique token so the
		// snapshot key (and its paired APPLIED_PREFIX key) stay unique while the
		// leading time() int still drives chronological ordering.
		$suffix = sprintf( '%d_%s', time(), substr( uniqid( '', true ), -6 ) );
		add_post_meta( $post_id, self::SNAPSHOT_PREFIX . $suffix, $content );
		$keys = $this->snapshot_keys( $post_id );
		while ( count( $keys ) > self::SNAPSHOTS_TO_KEEP ) {
			$oldest = array_shift( $keys );
			delete_post_meta( $post_id, $oldest );
			$old_suffix = substr( $oldest, strlen( self::SNAPSHOT_PREFIX ) );
			delete_post_meta( $post_id, self::APPLIED_PREFIX . $old_suffix );
		}
		return $suffix;
	}

	/**
	 * All snapshot meta keys for a post, oldest first.
	 *
	 * @param int $post_id Post ID.
	 * @return string[]
	 */
	private function snapshot_keys( $post_id ) {
		$all  = get_post_meta( $post_id );
		$keys = array();
		if ( is_array( $all ) ) {
			foreach ( array_keys( $all ) as $key ) {
				if ( 0 === strpos( $key, self::SNAPSHOT_PREFIX ) ) {
					$keys[] = $key;
				}
			}
		}
		usort(
			$keys,
			static function ( $a, $b ) {
				$prefix_len = strlen( self::SNAPSHOT_PREFIX );
				$suffix_a   = substr( $a, $prefix_len );
				$suffix_b   = substr( $b, $prefix_len );
				// Order by the leading time() int first, then by the full suffix
				// string so same-second snapshots still sort deterministically.
				$time_a = (int) $suffix_a;
				$time_b = (int) $suffix_b;
				if ( $time_a !== $time_b ) {
					return $time_a <=> $time_b;
				}
				return strcmp( $suffix_a, $suffix_b );
			}
		);
		return $keys;
	}
}
