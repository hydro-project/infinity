import { DynamoDBClient, ScanCommand, UpdateItemCommand } from '@aws-sdk/client-dynamodb';
import { sendSubscriptionEvent } from 'rap-js';
import { XMLParser } from 'fast-xml-parser';

const dynamoClient = new DynamoDBClient({});
const xmlParser = new XMLParser();

const SUBSCRIPTIONS_TABLE = process.env.SUBSCRIPTIONS_TABLE;

// --- Price helpers ---

async function fetchPrice(symbol) {
  const url = `https://query2.finance.yahoo.com/v8/finance/chart/${encodeURIComponent(symbol)}?range=1d&interval=1d`;
  const res = await fetch(url, {
    headers: { 'User-Agent': 'Mozilla/5.0' },
  });
  if (!res.ok) throw new Error(`Yahoo Finance returned ${res.status} for ${symbol}`);
  const data = await res.json();
  const meta = data.chart?.result?.[0]?.meta;
  if (!meta) throw new Error(`No data for ${symbol}`);
  return meta.regularMarketPrice;
}

async function processPriceSubscriptions(items) {
  const bySymbol = {};
  for (const item of items) {
    const sym = item.symbol.S;
    if (!bySymbol[sym]) bySymbol[sym] = [];
    bySymbol[sym].push(item);
  }

  for (const [symbol, subs] of Object.entries(bySymbol)) {
    let price;
    try {
      price = await fetchPrice(symbol);
    } catch (err) {
      console.error(`Failed to fetch price for ${symbol}:`, err.message);
      continue;
    }

    for (const sub of subs) {
      const lastPrice = parseFloat(sub.lastPrice?.N || '0');
      const threshold = parseFloat(sub.threshold.N);

      await dynamoClient.send(new UpdateItemCommand({
        TableName: SUBSCRIPTIONS_TABLE,
        Key: { pk: { S: sub.pk.S }, sk: { S: sub.sk.S } },
        UpdateExpression: 'SET lastPrice = :p',
        ExpressionAttributeValues: { ':p': { N: String(price) } },
      }));

      if (lastPrice === 0) continue;

      const change = Math.abs(price - lastPrice);
      if (change >= threshold) {
        const direction = price > lastPrice ? 'up' : 'down';
        const text = JSON.stringify({
          event: 'price_change',
          symbol,
          previous_price: lastPrice,
          current_price: price,
          change: parseFloat(change.toFixed(2)),
          direction,
          threshold,
        });

        await sendSubscriptionEvent(
          sub.rapReceiverUrl.S,
          sub.groupId.S,
          sub.toolCallId.S,
          text,
        );
      }
    }
  }
}

// --- News helpers ---

async function fetchNews(query) {
  const url = `https://news.google.com/rss/search?q=${encodeURIComponent(query)}&hl=en-US&gl=US&ceid=US:en`;
  const res = await fetch(url, {
    headers: { 'User-Agent': 'Mozilla/5.0' },
  });
  if (!res.ok) throw new Error(`Google News RSS returned ${res.status}`);
  const xml = await res.text();
  const parsed = xmlParser.parse(xml);
  const items = parsed?.rss?.channel?.item;
  if (!items) return [];
  return Array.isArray(items) ? items : [items];
}

async function processNewsSubscriptions(items) {
  const byQuery = {};
  for (const item of items) {
    const q = item.query.S;
    if (!byQuery[q]) byQuery[q] = [];
    byQuery[q].push(item);
  }

  for (const [query, subs] of Object.entries(byQuery)) {
    let articles;
    try {
      articles = await fetchNews(query);
    } catch (err) {
      console.error(`Failed to fetch news for "${query}":`, err.message);
      continue;
    }

    if (articles.length === 0) continue;

    for (const sub of subs) {
      const lastId = sub.lastArticleId?.S || '';
      const latestId = articles[0].guid || articles[0].link || articles[0].title || '';

      await dynamoClient.send(new UpdateItemCommand({
        TableName: SUBSCRIPTIONS_TABLE,
        Key: { pk: { S: sub.pk.S }, sk: { S: sub.sk.S } },
        UpdateExpression: 'SET lastArticleId = :a',
        ExpressionAttributeValues: { ':a': { S: latestId } },
      }));

      if (!lastId) continue;
      if (lastId === latestId) continue;

      const newArticles = [];
      for (const a of articles) {
        const aid = a.guid || a.link || a.title || '';
        if (aid === lastId) break;
        newArticles.push({ title: a.title, link: a.link, pubDate: a.pubDate, source: a.source });
      }

      if (newArticles.length === 0) continue;

      const toSend = newArticles.slice(0, 5);
      const text = JSON.stringify({
        event: 'news_update',
        query,
        articles: toSend,
      });

      await sendSubscriptionEvent(
        sub.rapReceiverUrl.S,
        sub.groupId.S,
        sub.toolCallId.S,
        text,
      );
    }
  }
}

export const handler = async () => {
  const result = await dynamoClient.send(new ScanCommand({
    TableName: SUBSCRIPTIONS_TABLE,
  }));

  const items = result.Items || [];
  const priceItems = items.filter(i => i.subType?.S === 'price');
  const newsItems = items.filter(i => i.subType?.S === 'news');

  console.log(`Processing ${priceItems.length} price subs, ${newsItems.length} news subs`);

  await Promise.all([
    processPriceSubscriptions(priceItems),
    processNewsSubscriptions(newsItems),
  ]);

  return { statusCode: 200 };
};
