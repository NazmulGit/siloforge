<?php
/**
 * Content sync: pushes published posts/pages to the SILOFORGE backend.
 *
 * @package siloforge-connect
 */

if ( ! defined( 'ABSPATH' ) ) {
	exit;
}

/**
 * Builds SyncPostPayload batches and POSTs them to /api/v1/sync.
 */
class SFC_Sync {

	/**
	 * Hard upper bound on the number of posts in a single batch (secondary
	 * bound; the byte budget below is the primary flush trigger).
	 */
	const BATCH_SIZE = 50;

	/**
	 * Byte budget for a single serialized sync batch. Kept well under the
	 * backend body limit (25 MB) so a content-heavy site never trips a 413:
	 * a batch is flushed before adding a post that would push the serialized
	 * payload past this size.
	 */
	const MAX_BATCH_BYTES = 3 * 1024 * 1024;

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
	 * Sync all published posts and pages.
	 *
	 * Posts are accumulated into a batch and flushed (POSTed to /api/v1/sync)
	 * when adding the next post would push the serialized payload past
	 * self::MAX_BATCH_BYTES, or when the batch reaches self::BATCH_SIZE posts,
	 * whichever comes first. This keeps a single batch comfortably under the
	 * backend body limit even for content-heavy sites. A single post larger
	 * than the byte budget still goes out on its own (one post can't be split).
	 *
	 * @return array|WP_Error Totals: posts, batches, received, upserted, skipped.
	 */
	public function sync_all() {
		$domain = (string) wp_parse_url( home_url(), PHP_URL_HOST );
		$totals = array(
			'posts'    => 0,
			'batches'  => 0,
			'received' => 0,
			'upserted' => 0,
			'skipped'  => 0,
		);

		$batch       = array();
		$batch_bytes = 0;

		$paged = 1;
		$more  = false;
		do {
			$query = new WP_Query(
				array(
					'post_type'           => array( 'post', 'page' ),
					'post_status'         => 'publish',
					'posts_per_page'      => self::BATCH_SIZE,
					'paged'               => $paged,
					'orderby'             => 'ID',
					'order'               => 'ASC',
					'ignore_sticky_posts' => true,
					'no_found_rows'       => true,
				)
			);
			if ( empty( $query->posts ) ) {
				break;
			}

			foreach ( $query->posts as $post ) {
				$item      = $this->build_payload( $post );
				$item_size = strlen( wp_json_encode( $item ) );

				// Flush the current batch before adding a post that would exceed
				// the byte budget or the count cap. Only flush when the batch is
				// non-empty so an oversized single post still gets sent.
				if ( ! empty( $batch )
					&& ( count( $batch ) >= self::BATCH_SIZE
						|| ( $batch_bytes + $item_size ) > self::MAX_BATCH_BYTES )
				) {
					$flushed = $this->flush_batch( $domain, $batch, $totals );
					if ( is_wp_error( $flushed ) ) {
						return $flushed;
					}
					$batch       = array();
					$batch_bytes = 0;
				}

				$batch[]      = $item;
				$batch_bytes += $item_size;
			}

			$more = ( count( $query->posts ) === self::BATCH_SIZE );
			$paged++;
		} while ( $more );

		// Flush the trailing partial batch.
		if ( ! empty( $batch ) ) {
			$flushed = $this->flush_batch( $domain, $batch, $totals );
			if ( is_wp_error( $flushed ) ) {
				return $flushed;
			}
		}

		update_option(
			'sfc_last_sync',
			array(
				'time'   => gmdate( 'c' ),
				'totals' => $totals,
			),
			false
		);

		return $totals;
	}

	/**
	 * POST one accumulated batch to /api/v1/sync and fold the response into
	 * the running totals.
	 *
	 * @param string $domain Site domain.
	 * @param array  $batch  Batch of SyncPostPayload arrays.
	 * @param array  $totals Totals accumulator (by reference).
	 * @return true|WP_Error True on success, WP_Error on transport/API failure.
	 */
	private function flush_batch( $domain, array $batch, array &$totals ) {
		$response = $this->client->authed_request(
			'POST',
			'/api/v1/sync',
			array(
				'domain' => $domain,
				'posts'  => $batch,
			)
		);
		if ( is_wp_error( $response ) ) {
			return $response;
		}

		$totals['posts']   += count( $batch );
		$totals['batches'] += 1;
		foreach ( array( 'received', 'upserted', 'skipped' ) as $key ) {
			if ( isset( $response[ $key ] ) ) {
				$totals[ $key ] += (int) $response[ $key ];
			}
		}

		return true;
	}

	/**
	 * Build one SyncPostPayload array for a post.
	 *
	 * @param WP_Post $post Post object.
	 * @return array
	 */
	private function build_payload( WP_Post $post ) {
		$payload = array(
			'wp_post_id'   => (int) $post->ID,
			'url'          => (string) get_permalink( $post ),
			'slug'         => (string) $post->post_name,
			'title'        => (string) get_the_title( $post ),
			'content_html' => (string) apply_filters( 'the_content', $post->post_content ),
			'post_type'    => (string) $post->post_type,
			'post_status'  => (string) $post->post_status,
			'builder'      => $this->detect_builder( $post ),
		);

		$meta_description = $this->meta_description( $post );
		if ( '' !== $meta_description ) {
			$payload['meta_description'] = $meta_description;
		}

		$published_at = $this->to_iso8601( $post->post_date_gmt );
		if ( '' !== $published_at ) {
			$payload['published_at'] = $published_at;
		}

		$modified_at = $this->to_iso8601( $post->post_modified_gmt );
		if ( '' !== $modified_at ) {
			$payload['modified_at'] = $modified_at;
		}

		return $payload;
	}

	/**
	 * Detect which builder produced the post content.
	 *
	 * @param WP_Post $post Post object.
	 * @return string One of 'elementor' | 'divi' | 'gutenberg' | 'classic'.
	 */
	private function detect_builder( WP_Post $post ) {
		if ( 'builder' === get_post_meta( $post->ID, '_elementor_edit_mode', true ) ) {
			return 'elementor';
		}
		if ( 'on' === get_post_meta( $post->ID, '_et_pb_use_builder', true ) ) {
			return 'divi';
		}
		if ( function_exists( 'has_blocks' ) && has_blocks( $post ) ) {
			return 'gutenberg';
		}
		return 'classic';
	}

	/**
	 * Meta description: Yoast field with excerpt fallback.
	 *
	 * @param WP_Post $post Post object.
	 * @return string
	 */
	private function meta_description( WP_Post $post ) {
		$description = get_post_meta( $post->ID, '_yoast_wpseo_metadesc', true );
		if ( ! is_string( $description ) || '' === trim( $description ) ) {
			$description = (string) $post->post_excerpt;
		}
		return trim( wp_strip_all_tags( $description ) );
	}

	/**
	 * Convert a GMT MySQL datetime to ISO 8601 (UTC).
	 *
	 * @param string $mysql_gmt MySQL datetime string (GMT).
	 * @return string Empty string when the date is missing/invalid.
	 */
	private function to_iso8601( $mysql_gmt ) {
		if ( empty( $mysql_gmt ) || '0000-00-00 00:00:00' === $mysql_gmt ) {
			return '';
		}
		$timestamp = strtotime( $mysql_gmt . ' UTC' );
		if ( false === $timestamp ) {
			return '';
		}
		return gmdate( 'Y-m-d\TH:i:s\Z', $timestamp );
	}
}
